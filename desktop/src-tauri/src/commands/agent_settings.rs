use std::sync::atomic::Ordering;
use tauri::{AppHandle, Manager, State};

use crate::{
    app_state::AppState,
    managed_agents::{
        begin_backend_transition, current_instance_id, find_managed_agent_mut, load_managed_agents,
        load_personas, resolve_provider_binary, retire_deployment_pointer, save_managed_agents,
        sync_managed_agent_processes, validate_backend_migration, validate_provider_config,
        BackendKind, ManagedAgentSummary, MigrationPreconditions,
    },
    util::now_iso,
};

#[tauri::command]
pub fn set_agent_managed_profiles(enabled: bool, state: State<'_, AppState>) {
    state
        .managed_agent_profile_reconcile_enabled()
        .store(!enabled, Ordering::Release);
}

#[tauri::command]
pub fn set_thread_scoped_acp_sessions(enabled: bool, state: State<'_, AppState>) {
    state
        .thread_scoped_acp_sessions_enabled()
        .store(enabled, Ordering::Release);
}

#[tauri::command]
pub async fn set_managed_agent_start_on_app_launch(
    pubkey: String,
    start_on_app_launch: bool,
    app: AppHandle,
) -> Result<ManagedAgentSummary, String> {
    tokio::task::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let _store_guard = state
            .managed_agents_store_lock
            .lock()
            .map_err(|error| error.to_string())?;
        let mut records = load_managed_agents(&app)?;
        let mut runtimes = state
            .managed_agent_processes
            .lock()
            .map_err(|error| error.to_string())?;

        let (sync_changed, exited_pubkeys) =
            sync_managed_agent_processes(&mut records, &mut runtimes, &current_instance_id(&app));
        if sync_changed {
            save_managed_agents(&app, &records)?;
        }
        for pubkey in &exited_pubkeys {
            state.clear_agent_session_caches(pubkey);
        }

        {
            let record = find_managed_agent_mut(&mut records, &pubkey)?;
            record.start_on_app_launch = start_on_app_launch;
            record.updated_at = now_iso();
        }

        save_managed_agents(&app, &records)?;
        let record = records
            .iter()
            .find(|record| record.pubkey == pubkey)
            .ok_or_else(|| format!("agent {pubkey} not found"))?;
        // A config change like any other update path — see `set_managed_agent_waker_enabled`'s
        // doc — so it must reissue a waker-enrolled agent's launch bundle too, or the
        // "change any setting to reissue" recovery copy in `wakerBundleHealth.ts` would be
        // false for this specific setting.
        super::agents::retain_managed_agent_pending(&app, &state, record);
        super::agents::summarize_from_disk(&app, record, &runtimes)
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {e}"))?
}

/// Move an existing agent between running locally and running on a backend
/// provider, **preserving its identity**: pubkey, keypair, channel grants, git
/// ACL, auth tag, and NIP-AE engrams all follow the record. Delete-and-recreate
/// is the only alternative and it destroys every one of those permanently —
/// there is no key import.
///
/// This deliberately is NOT a field on `UpdateManagedAgentRequest`: an ordinary
/// edit-dialog save must never be able to change where an agent runs.
///
/// # The invariant
///
/// **One identity, one live harness.** Two harnesses on one key means doubled
/// replies, flapping presence, and concurrent engram writes against the same
/// `(agent, owner)` pair. Everything below exists to prevent that.
///
/// The two directions are not symmetric, because liveness is only observable
/// for local agents:
///
/// - **Local**: the runtime map and this instance's pair receipts track real
///   children, so a surviving pair is authoritative. Enforced here via
///   `local_harness_alive`. Note `record.runtime_pid` is *not* usable for this
///   — `sync_managed_agent_processes` clears it as legacy bookkeeping on every
///   record it touches, which is exactly what made the first version of this
///   guard read false for a healthy running agent.
/// - **Provider**: [`crate::managed_agents::build_managed_agent_summary`] reports `deployed` /
///   `not_deployed` from `backend_agent_id` — that is *infrastructure
///   existence*, not liveness. A sprite stays "deployed" after `!shutdown`, and
///   relay presence (the real signal) is polled by the frontend and never
///   reaches this process. So this command **cannot verify** that a remote
///   harness has stopped, and says so by requiring the caller to assert it via
///   `remote_confirmed_stopped`.
///
/// `stop_managed_agent` rejects non-local backends outright ("remote agents are
/// stopped via !shutdown message"), so there is no backend call that could make
/// the assertion true — only the user can, by sending `!shutdown` and watching
/// the PresenceDot go offline.
///
/// `remote_confirmed_stopped` is that assertion, made by the caller against
/// relay presence. It is required only when leaving a provider backend and
/// ignored otherwise.
///
/// # Serialization against provider deploys
///
/// Both this command and `deploy_to_provider` take the per-agent
/// [`crate::managed_agents::begin_backend_transition`] fence before the store
/// lock. Without it, a deploy — which runs outside the store lock and may take
/// minutes — could complete after a move to Local had already been persisted,
/// starting a remote harness for a record that then also permits a local one.
///
/// # Deployments left behind
///
/// A provider deployment survives the agent moving off it (Buzz has no
/// `undeploy`) and keeps a copy of the private key. Its id is retired into
/// `residual_deployments` alongside the provider that issued it, so
/// `delete_managed_agent` can still see it and demand `force_remote_delete`.
#[tauri::command]
pub async fn set_managed_agent_backend(
    pubkey: String,
    backend: BackendKind,
    remote_confirmed_stopped: bool,
    app: AppHandle,
) -> Result<ManagedAgentSummary, String> {
    // Validate the target BEFORE taking any lock or touching the store, exactly
    // as `create_managed_agent` does in its pre-phase: an unreachable provider
    // binary must fail with the record untouched.
    if let BackendKind::Provider { ref id, ref config } = backend {
        validate_provider_config(config)?;
        resolve_provider_binary(id)?;
    }

    tokio::task::spawn_blocking(move || {
        let state = app.state::<AppState>();

        // Fence first, store lock second — always this order, everywhere (see
        // `BackendTransitionGuard`). A provider deploy started by
        // `start_managed_agent` releases the store lock across the external
        // call, so the lock alone cannot keep this move from landing inside a
        // deploy that then starts a remote harness for a now-Local record.
        let _transition = begin_backend_transition(&pubkey)?;

        let _store_guard = state
            .managed_agents_store_lock
            .lock()
            .map_err(|error| error.to_string())?;
        let mut records = load_managed_agents(&app)?;
        let mut runtimes = state
            .managed_agent_processes
            .lock()
            .map_err(|error| error.to_string())?;

        let (sync_changed, exited_pubkeys) =
            sync_managed_agent_processes(&mut records, &mut runtimes, &current_instance_id(&app));
        if sync_changed {
            save_managed_agents(&app, &records)?;
        }
        for pubkey in &exited_pubkeys {
            state.clear_agent_session_caches(pubkey);
        }

        let personas = load_personas(&app).unwrap_or_default();
        let global = crate::managed_agents::load_global_agent_config(&app).unwrap_or_default();

        let leaving_provider_waker;
        {
            let record = find_managed_agent_mut(&mut records, &pubkey)?;

            // Liveness comes from the runtime map and the pair receipts, never
            // from `record.runtime_pid` — `sync_managed_agent_processes` above
            // clears that field on every record before this line runs. There
            // is no equivalent signal for remote harnesses, hence
            // `remote_confirmed_stopped`.
            //
            // Shared compute likewise resolves through the effective config:
            // `record.relay_mesh` is a legacy marker that a linked instance or
            // a global default can contradict in either direction.
            let observed = MigrationPreconditions {
                local_harness_alive: crate::managed_agents::local_harness_alive(
                    &app, &runtimes, &pubkey,
                ),
                remote_confirmed_stopped,
                uses_relay_mesh:
                    crate::managed_agents::effective_config::resolve_effective_relay_mesh_model_id(
                        record, &personas, &global,
                    )
                    .is_some(),
            };
            validate_backend_migration(&record.backend, &backend, &observed)?;

            // `set_managed_agent_waker_enabled` refuses to enable buzz-waker
            // for anything but a `Provider` backend, so a still-enabled agent
            // migrating off one is leaving the only backend its retained
            // launch bundle authorizes. Force it off and revoke below —
            // otherwise the stale bundle would keep authorizing a remote
            // deploy for a backend this agent no longer runs on.
            leaving_provider_waker =
                record.waker_enabled && !matches!(backend, BackendKind::Provider { .. });

            record.provider_binary_path = match backend {
                // Cache the discovered path for `deploy_to_provider`, the same
                // way creation does.
                BackendKind::Provider { ref id, .. } => resolve_provider_binary(id)
                    .ok()
                    .map(|p| p.display().to_string()),
                BackendKind::Local => None,
            };

            // The deployment the agent is leaving still exists and still holds
            // a copy of its key, so its id is retired into
            // `residual_deployments` *with the provider and config that issued
            // it* rather than left behind on `backend_agent_id`, where nothing
            // could tell which provider — or which deployment scope within that
            // provider — it belonged to. Must run before `record.backend` is
            // overwritten: the old backend is the thing being retired.
            retire_deployment_pointer(
                &record.backend,
                &backend,
                &mut record.backend_agent_id,
                &mut record.residual_deployments,
            );

            // Provider agents are managed externally — creation forces this
            // false and so must we, or a migrated agent would try to launch
            // locally at next app start.
            record.start_on_app_launch = false;
            record.backend = backend;
            record.updated_at = now_iso();
            if leaving_provider_waker {
                record.waker_enabled = false;
            }
        }

        if leaving_provider_waker {
            // Durable revoke, BEFORE the migration is persisted: a failure
            // here must abort the whole migration (the in-memory mutation
            // above is simply discarded, since it was never saved) rather
            // than let a `Local` record land with the old `Provider` bundle
            // still live for up to 90 days. See `revoke_waker_bundle_pending`'s
            // own doc.
            super::agents::revoke_waker_bundle_pending(&app, &state, &pubkey).map_err(|e| {
                format!(
                    "buzz-waker could not revoke the launch bundle; backend migration aborted: {e}"
                )
            })?;
            // Withdraw enrolment too, and for the same reason: revoking only
            // the bundle would leave the daemon still watching this agent and
            // still holding its key, for an agent that no longer runs there.
            super::agents_waker_enrolment::revoke_waker_enrolment_pending(&app, &state, &pubkey)
                .map_err(|e| {
                    format!(
                        "buzz-waker could not withdraw enrolment; backend migration aborted: {e}"
                    )
                })?;
        }

        save_managed_agents(&app, &records)?;

        let record = records
            .iter()
            .find(|record| record.pubkey == pubkey)
            .ok_or_else(|| format!("agent {pubkey} not found"))?;
        // Same reissue as every other settings mutator (see
        // `set_managed_agent_start_on_app_launch`). A no-op when `leaving_provider_waker`
        // already revoked above, since `record.waker_enabled` now reads false; otherwise
        // this is the path that picks up the new `provider_binary_path` a same-provider
        // config change or a re-migration onto `Provider` just wrote.
        super::agents::retain_managed_agent_pending(&app, &state, record);
        super::agents::summarize_from_disk(&app, record, &runtimes)
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {e}"))?
}

/// Assign a managed agent to a community (`Some`) or unscope it (`None` =
/// offered in every community). Display/uniqueness scope only — never
/// affects where the agent can run.
#[tauri::command]
pub async fn set_managed_agent_community(
    pubkey: String,
    community_relay_url: Option<String>,
    app: AppHandle,
) -> Result<ManagedAgentSummary, String> {
    // Unlike creation (which degrades to unscoped on a bad workspace relay),
    // an explicit assignment that fails to normalize is a hard error —
    // silently unscoping a value the user picked would be a lie.
    let community_relay_url = match community_relay_url {
        Some(url) => Some(
            buzz_core_pkg::relay::normalize_relay_url(&url)
                .map_err(|error| format!("invalid community relay URL: {error}"))?,
        ),
        None => None,
    };

    tokio::task::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let _store_guard = state
            .managed_agents_store_lock
            .lock()
            .map_err(|error| error.to_string())?;
        let mut records = load_managed_agents(&app)?;
        let mut runtimes = state
            .managed_agent_processes
            .lock()
            .map_err(|error| error.to_string())?;

        let (sync_changed, exited_pubkeys) =
            sync_managed_agent_processes(&mut records, &mut runtimes, &current_instance_id(&app));
        if sync_changed {
            save_managed_agents(&app, &records)?;
        }
        for pubkey in &exited_pubkeys {
            state.clear_agent_session_caches(pubkey);
        }

        {
            let record = find_managed_agent_mut(&mut records, &pubkey)?;
            let name = record.name.clone();
            // The same per-(community, name) rule as creation, against the
            // TARGET scope and excluding this record itself — assigning an
            // agent into a community that already offers that name would
            // manufacture exactly the ambiguity scoping removes.
            if crate::managed_agents::instance_name_taken_in_scope(
                &records,
                &name,
                community_relay_url.as_deref(),
                Some(&pubkey),
            ) {
                return Err(format!(
                    "an agent named \"{name}\" already exists in this community"
                ));
            }
            let record = find_managed_agent_mut(&mut records, &pubkey)?;
            record.community_relay_url = community_relay_url;
            record.updated_at = now_iso();
        }

        save_managed_agents(&app, &records)?;
        let record = records
            .iter()
            .find(|record| record.pubkey == pubkey)
            .ok_or_else(|| format!("agent {pubkey} not found"))?;
        // See the matching reissue call in `set_managed_agent_start_on_app_launch` above:
        // this is a user-facing settings mutator, so the "change any setting to reissue"
        // copy in `wakerBundleHealth.ts` must hold here too.
        super::agents::retain_managed_agent_pending(&app, &state, record);
        super::agents::summarize_from_disk(&app, record, &runtimes)
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {e}"))?
}

/// Opt an agent into (or out of) `buzz-waker` deployment.
///
/// Turning this on for a `Provider`-backend agent is the enrolment moment
/// (`PLANS/BUZZ_WAKER_DESIGN.md` §11): this issues and retains both that
/// agent's first signed launch bundle and its first enrolment credential
/// (the roster entry that lets the daemon discover it at all) in the same
/// call. Every later edit that already calls `retain_managed_agent_pending`
/// (model/provider changes, persona-propagated updates, rollback restores)
/// reissues both — never a bare liveness ping (G3).
///
/// Refused for a `Local` backend: there is nothing for a remote daemon to
/// invoke, so an enabled flag would sit there silently doing nothing.
#[tauri::command]
pub async fn set_managed_agent_waker_enabled(
    pubkey: String,
    waker_enabled: bool,
    app: AppHandle,
) -> Result<ManagedAgentSummary, String> {
    tokio::task::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let _store_guard = state
            .managed_agents_store_lock
            .lock()
            .map_err(|error| error.to_string())?;
        let mut records = load_managed_agents(&app)?;
        let mut runtimes = state
            .managed_agent_processes
            .lock()
            .map_err(|error| error.to_string())?;

        let (sync_changed, exited_pubkeys) =
            sync_managed_agent_processes(&mut records, &mut runtimes, &current_instance_id(&app));
        if sync_changed {
            save_managed_agents(&app, &records)?;
        }
        for pubkey in &exited_pubkeys {
            state.clear_agent_session_caches(pubkey);
        }

        let was_enabled = {
            let record = find_managed_agent_mut(&mut records, &pubkey)?;
            if waker_enabled && !matches!(record.backend, BackendKind::Provider { .. }) {
                return Err(
                    "buzz-waker can only deploy agents running on a provider backend".to_string(),
                );
            }
            record.waker_enabled
        };

        if was_enabled && !waker_enabled {
            // Durable revoke, BEFORE the flag is persisted as disabled: see
            // `revoke_waker_bundle_pending`'s own doc for why this reaches an
            // already-connected daemon, not just a future relay recovery, and
            // why a failure here must refuse the whole transition rather than
            // report success with the old bundle still live. Failing before
            // any mutation needs no rollback — nothing has been written yet.
            super::agents::revoke_waker_bundle_pending(&app, &state, &pubkey).map_err(|e| {
                format!("buzz-waker could not revoke the launch bundle; waker remains enabled: {e}")
            })?;
            // Turning the toggle off must also withdraw enrolment, or the
            // daemon keeps watching this agent and keeps its key — the bundle
            // revocation alone only stops it deploying one.
            super::agents_waker_enrolment::revoke_waker_enrolment_pending(&app, &state, &pubkey)
                .map_err(|e| {
                    format!("buzz-waker could not withdraw enrolment; waker remains enabled: {e}")
                })?;
        }

        {
            let record = find_managed_agent_mut(&mut records, &pubkey)?;
            record.waker_enabled = waker_enabled;
            record.updated_at = now_iso();
        }
        save_managed_agents(&app, &records)?;

        if waker_enabled && !was_enabled {
            // Enrolment: a swallowed issuance failure here would report
            // success while buzz-waker has nothing to deploy, and — per G3
            // (never a bare liveness ping) — nothing else would retry it.
            // Fail the command and roll the flag back so a retry re-enters
            // this same transition rather than looking like a completed
            // no-op.
            let record = records
                .iter()
                .find(|record| record.pubkey == pubkey)
                .ok_or_else(|| format!("agent {pubkey} not found"))?;
            if let Err(e) = super::agents::retain_waker_bundle_pending(&app, &state, record) {
                let record = find_managed_agent_mut(&mut records, &pubkey)?;
                record.waker_enabled = false;
                record.updated_at = now_iso();
                save_managed_agents(&app, &records)?;
                return Err(format!(
                    "buzz-waker enrolment failed to issue a launch bundle: {e}"
                ));
            }
            // The bundle alone only tells the daemon how to deploy this agent
            // once it already knows to watch it. Without also issuing the
            // enrolment credential and roster entry here, the daemon never
            // discovers a freshly enabled agent unless some unrelated later
            // settings mutation happens to call the generic retain helper
            // that covers both (`retain_managed_agent_pending`).
            if let Err(e) =
                super::agents_waker_enrolment::retain_waker_enrolment_pending(&app, &state, record)
            {
                let record = find_managed_agent_mut(&mut records, &pubkey)?;
                record.waker_enabled = false;
                record.updated_at = now_iso();
                save_managed_agents(&app, &records)?;
                return Err(format!(
                    "buzz-waker enrolment failed to issue watch credentials: {e}"
                ));
            }
        } else {
            let record = records
                .iter()
                .find(|record| record.pubkey == pubkey)
                .ok_or_else(|| format!("agent {pubkey} not found"))?;
            super::agents::retain_managed_agent_pending(&app, &state, record);
        }

        let record = records
            .iter()
            .find(|record| record.pubkey == pubkey)
            .ok_or_else(|| format!("agent {pubkey} not found"))?;
        super::agents::summarize_from_disk(&app, record, &runtimes)
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {e}"))?
}

#[tauri::command]
pub async fn set_managed_agent_auto_restart(
    pubkey: String,
    auto_restart_on_config_change: bool,
    app: AppHandle,
) -> Result<ManagedAgentSummary, String> {
    tokio::task::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let _store_guard = state
            .managed_agents_store_lock
            .lock()
            .map_err(|error| error.to_string())?;
        let mut records = load_managed_agents(&app)?;
        let mut runtimes = state
            .managed_agent_processes
            .lock()
            .map_err(|error| error.to_string())?;

        let (sync_changed, exited_pubkeys) =
            sync_managed_agent_processes(&mut records, &mut runtimes, &current_instance_id(&app));
        if sync_changed {
            save_managed_agents(&app, &records)?;
        }
        for pubkey in &exited_pubkeys {
            state.clear_agent_session_caches(pubkey);
        }

        {
            let record = find_managed_agent_mut(&mut records, &pubkey)?;
            record.auto_restart_on_config_change = auto_restart_on_config_change;
            record.updated_at = now_iso();
        }

        save_managed_agents(&app, &records)?;
        let record = records
            .iter()
            .find(|record| record.pubkey == pubkey)
            .ok_or_else(|| format!("agent {pubkey} not found"))?;
        // See the matching reissue call in `set_managed_agent_start_on_app_launch` above.
        super::agents::retain_managed_agent_pending(&app, &state, record);
        super::agents::summarize_from_disk(&app, record, &runtimes)
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {e}"))?
}
