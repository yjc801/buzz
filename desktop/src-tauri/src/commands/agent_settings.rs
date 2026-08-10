use std::sync::atomic::Ordering;
use tauri::{AppHandle, Manager, State};

use crate::{
    app_state::AppState,
    managed_agents::{
        build_managed_agent_summary, current_instance_id, find_managed_agent_mut,
        load_managed_agents, load_personas, resolve_provider_binary, save_managed_agents,
        sync_managed_agent_processes, validate_backend_migration, validate_provider_config,
        BackendKind, ManagedAgentSummary, MigrationPreconditions,
    },
    util::now_iso,
};

#[tauri::command]
pub fn set_agent_managed_profiles(enabled: bool, state: State<'_, AppState>) {
    state
        .managed_agent_profile_reconcile_enabled
        .store(!enabled, Ordering::Release);
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
        let personas = load_personas(&app).unwrap_or_default();
        build_managed_agent_summary(
            &app,
            record,
            &runtimes,
            &personas,
            &crate::managed_agents::load_global_agent_config(&app).unwrap_or_default(),
        )
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
/// - **Local**: `sync_managed_agent_processes` reconciles real processes, so a
///   surviving pair or live pid is authoritative. Enforced here.
/// - **Provider**: [`build_managed_agent_summary`] reports `deployed` /
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

            // `sync_managed_agent_processes` has already reconciled the record
            // against real processes, so a surviving pid is authoritative for
            // local agents. There is no equivalent signal for remote ones —
            // hence `remote_confirmed_stopped`.
            let observed = MigrationPreconditions {
                local_process_alive: record
                    .runtime_pid
                    .is_some_and(crate::managed_agents::process_is_running),
                remote_confirmed_stopped,
                uses_relay_mesh: record.relay_mesh.is_some(),
            };
            validate_backend_migration(&record.backend, &backend, &observed)?;

            record.provider_binary_path = match backend {
                // Cache the discovered path for `deploy_to_provider`, the same
                // way creation does.
                BackendKind::Provider { ref id, .. } => {
                    resolve_provider_binary(id).ok().map(|p| p.display().to_string())
                }
                BackendKind::Local => None,
            };

            // `backend_agent_id` is deliberately preserved when leaving a
            // provider: it is the only pointer back to infrastructure that
            // still exists and still holds a copy of this agent's key. Clearing
            // it would strand the deployment with nothing able to name it. It
            // is ignored for display while the backend is Local.

            // Provider agents are managed externally — creation forces this
            // false and so must we, or a migrated agent would try to launch
            // locally at next app start.
            record.start_on_app_launch = false;
            record.backend = backend;
            record.updated_at = now_iso();
        }

        save_managed_agents(&app, &records)?;
        let record = records
            .iter()
            .find(|record| record.pubkey == pubkey)
            .ok_or_else(|| format!("agent {pubkey} not found"))?;
        let personas = load_personas(&app).unwrap_or_default();
        build_managed_agent_summary(
            &app,
            record,
            &runtimes,
            &personas,
            &crate::managed_agents::load_global_agent_config(&app).unwrap_or_default(),
        )
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
        let personas = load_personas(&app).unwrap_or_default();
        build_managed_agent_summary(
            &app,
            record,
            &runtimes,
            &personas,
            &crate::managed_agents::load_global_agent_config(&app).unwrap_or_default(),
        )
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
        let personas = load_personas(&app).unwrap_or_default();
        build_managed_agent_summary(
            &app,
            record,
            &runtimes,
            &personas,
            &crate::managed_agents::load_global_agent_config(&app).unwrap_or_default(),
        )
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {e}"))?
}
