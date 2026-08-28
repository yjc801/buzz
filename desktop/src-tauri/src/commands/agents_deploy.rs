//! Provider deploy payload construction, split from `agents.rs` (file-size
//! guard). The launch block is derived from the same effective descriptor and
//! policy helpers as local spawn so remote execution does not reimplement them.

use std::collections::BTreeMap;
use std::sync::Arc;

use tauri::AppHandle;

#[cfg(test)]
use crate::managed_agents::AgentDefinition;
use crate::{
    app_state::AppState,
    managed_agents::{
        discover_provider_candidates, load_managed_agents, load_personas, provider_deploy,
        resolve_provider_binary, save_managed_agents, ManagedAgentRecord, ManagedAgentSummary,
    },
    relay::relay_ws_url_with_override,
    util::now_iso,
};

/// The start command's answer: the refreshed record summary plus, for
/// provider backends, the provider's own deploy classification (see
/// `ProviderDeployOutcome::fresh_generation`). Local starts carry `None`:
/// the only consumer is the wake path, which never targets local agents,
/// and a value here would claim provider proof that never existed.
#[derive(serde::Serialize)]
pub struct StartManagedAgentOutcome {
    pub agent: ManagedAgentSummary,
    pub fresh_generation: Option<bool>,
}

/// Deploy an agent to a provider backend. Resolves the binary, calls deploy via
/// spawn_blocking, and persists the result (backend_agent_id or last_error).
///
/// Idempotency: calling deploy on an already-deployed agent sends the same payload
/// again. Providers are expected to handle this as an update-in-place or no-op —
/// the protocol does not include an explicit `undeploy` operation (deferred to v2).
///
/// Returns the provider's fresh-generation classification on success
/// (`None` when the provider gave none — see `ProviderDeployOutcome`),
/// Err(message) on failure. Either way the record is updated and saved
/// before returning.
///
/// # Fenced against migration
///
/// Every caller resolves the provider under the store lock, releases it, and
/// only then calls this — and the deploy itself is an external process that
/// can run for minutes. `set_managed_agent_backend` could otherwise move the
/// agent to Local inside that window: the move lands first, this function
/// afterwards writes `backend_agent_id` and leaves a remote harness running
/// for a record that says Local, which then permits a second, local harness
/// on the same key.
///
/// So the per-agent transition fence is taken here, before the store lock and
/// before the external call, and the record's backend is re-read under it. A
/// migration that beat us aborts this deploy; one that arrives while the fence
/// is held is refused and can be retried. Only the provider *id* is compared —
/// a same-provider config change is an ordinary re-deploy that the deploy
/// itself reconciles.
///
/// # Serialized per agent
///
/// Two deploys for the same agent would otherwise race their record writes,
/// and the loser's `backend_agent_id` would overwrite the winner's. The
/// per-agent deploy lock is taken *before* the transition fence so a deploy
/// that queued behind another re-reads the backend below rather than acting on
/// what was true when it queued.
///
/// # Scoped to the caller's tenant
///
/// Callers with a captured tenant scope (Projects agent starts) pass
/// `expected_relay_url` / `expected_signer_pubkey`; they are asserted against
/// `agent_json` — the exact payload invoked below — after the deploy lock, so
/// a workspace or identity switch landing while this call waited behind
/// another deployment cannot deploy on behalf of a stale callback. `None`
/// preserves the unscoped behavior for callers without a tenant boundary.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn deploy_to_provider(
    app: &AppHandle,
    state: &AppState,
    pubkey: &str,
    provider_id: &str,
    config: &serde_json::Value,
    agent_json: serde_json::Value,
    cached_binary_path: Option<&str>,
    expected_relay_url: Option<&str>,
    expected_signer_pubkey: Option<&str>,
) -> Result<Option<bool>, String> {
    let deploy_lock = {
        let mut locks = state
            .provider_deploy_locks
            .lock()
            .map_err(|e| e.to_string())?;
        Arc::clone(
            locks
                .entry(pubkey.to_string())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    };
    let _deploy_guard = deploy_lock.lock().await;

    let _transition = crate::managed_agents::begin_backend_transition(pubkey)?;
    {
        let _store_guard = state
            .managed_agents_store_lock
            .lock()
            .map_err(|e| e.to_string())?;
        let records = load_managed_agents(app)?;
        let record = records
            .iter()
            .find(|r| r.pubkey == pubkey)
            .ok_or_else(|| format!("agent {pubkey} not found"))?;
        match &record.backend {
            crate::managed_agents::BackendKind::Provider { id, .. } if id == provider_id => {}
            _ => {
                return Err(format!(
                    "agent {pubkey} no longer runs on {provider_id} — it was moved while this \
                     deploy was starting"
                ))
            }
        }
    }

    // Tie the caller's captured scope to the use: this is the payload handed
    // to `provider_deploy` below, so a scoped caller can never land a deploy
    // outside the tenant and identity it validated.
    assert_payload_scope(&agent_json, expected_relay_url, expected_signer_pubkey)?;

    // Resolve via discovered candidates only. Cached path must match BOTH
    // "is a discovered candidate" AND "belongs to this provider_id". A tampered
    // record cannot redirect deploys to a different provider's binary.
    let bin_path = cached_binary_path
        .map(std::path::PathBuf::from)
        .filter(|p| p.exists())
        .map(|p| p.canonicalize().unwrap_or(p))
        .filter(|canonical| {
            discover_provider_candidates().iter().any(|(id, cp)| {
                id == provider_id && cp.canonicalize().ok().as_ref() == Some(canonical)
            })
        })
        .map_or_else(|| resolve_provider_binary(provider_id), Ok)?;

    let deployed_agent_json = agent_json.clone();
    let config_clone = config.clone();
    let deploy_result =
        tokio::task::spawn_blocking(move || provider_deploy(&bin_path, &agent_json, &config_clone))
            .await
            .map_err(|e| format!("spawn_blocking failed: {e}"))?;

    // Persist result under lock.
    let _store_guard = state
        .managed_agents_store_lock
        .lock()
        .map_err(|e| e.to_string())?;
    let mut records = load_managed_agents(app)?;
    let rec = records
        .iter_mut()
        .find(|r| r.pubkey == pubkey)
        .ok_or_else(|| format!("agent {pubkey} not found"))?;

    let fresh_generation = match deploy_result {
        Ok(outcome) => {
            // Deliberately does *not* clear a residual that looks like the
            // deployment this call just landed on. An id repeating after
            // A → Local → A cannot be told apart from the same id in another
            // cluster, because an omitted Kubernetes `context` resolves from the
            // machine's current kubeconfig at deploy time — see the note above
            // `deletion_orphans_infrastructure`. Retaining an entry that may be
            // this deployment costs a duplicate warning; dropping one that is
            // not loses the last pointer to a pod holding the private key.
            rec.backend_agent_id = Some(outcome.agent_id);
            acknowledge_policy(rec, &deployed_agent_json);
            rec.last_started_at = Some(now_iso());
            rec.updated_at = now_iso();
            rec.last_error = None;
            outcome.fresh_generation
        }
        Err(ref e) => {
            rec.last_error = Some(e.clone());
            rec.updated_at = now_iso();
            save_managed_agents(app, &records)?;
            return Err(e.clone());
        }
    };
    save_managed_agents(app, &records)?;
    Ok(fresh_generation)
}

/// Assert a caller-captured tenant scope against the payload that will
/// actually be invoked. The relay lives at the payload's top-level
/// `relay_url`; the deploying identity lives at `launch.owner_pubkey`. When
/// the caller carries an expectation a missing payload field fails closed: an
/// unverifiable payload must never deploy on behalf of a scoped callback.
fn assert_payload_scope(
    agent_json: &serde_json::Value,
    expected_relay_url: Option<&str>,
    expected_signer_pubkey: Option<&str>,
) -> Result<(), String> {
    let has_expectation =
        |expected: Option<&str>| expected.map(str::trim).filter(|s| !s.is_empty()).is_some();
    match agent_json.get("relay_url").and_then(|v| v.as_str()) {
        Some(embedded_relay) => crate::relay::assert_expected_relay_scope(
            expected_relay_url,
            &crate::relay::relay_http_base_url(embedded_relay),
        )?,
        None if has_expectation(expected_relay_url) => {
            return Err("deploy payload carries no relay; not deployed".to_string());
        }
        None => {}
    }
    match agent_json
        .get("launch")
        .and_then(|launch| launch.get("owner_pubkey"))
        .and_then(|v| v.as_str())
    {
        Some(owner) => crate::relay::assert_expected_signer(expected_signer_pubkey, owner)?,
        None if has_expectation(expected_signer_pubkey) => {
            return Err("deploy payload carries no owner identity; not deployed".to_string());
        }
        None => {}
    }
    Ok(())
}

/// Whether the access policy the provider actually received still matches the
/// record's current policy.
fn policy_matches_payload(
    record: &ManagedAgentRecord,
    deployed_agent_json: &serde_json::Value,
) -> bool {
    deployed_agent_json
        .get("respond_to")
        .and_then(serde_json::Value::as_str)
        == Some(record.respond_to.as_str())
        && deployed_agent_json.get("respond_to_allowlist")
            == Some(&serde_json::json!(record.respond_to_allowlist))
}

/// Clear `provider_policy_pending` only when this deploy actually carried the
/// record's current policy.
///
/// A deploy that queued behind another (or behind a policy edit made while it
/// was in flight) lands with a stale payload. Clearing unconditionally on
/// success would mark that older policy as acknowledged and drop the retry that
/// `provider_access::needs_reconciliation_with_policy` depends on, leaving the
/// provider running the superseded access rules.
fn acknowledge_policy(record: &mut ManagedAgentRecord, deployed_agent_json: &serde_json::Value) {
    if policy_matches_payload(record, deployed_agent_json) {
        record.provider_policy_pending = false;
    }
}

/// Effective projection fields for the deploy payload — all derived from the
/// resolved descriptor and effective config so that the serialised payload and
/// the `launch` block are always internally consistent.
pub(super) struct DeployProjections {
    pub effective_model: Option<String>,
    pub effective_provider: Option<String>,
    pub effective_prompt: Option<String>,
    /// Effective parallelism derived from the same resolved `descriptor.command`
    /// as `launch.policy_env["BUZZ_ACP_AGENTS"]`.
    pub effective_parallelism: u32,
    /// Access fields projected from the same build policy that gates local starts.
    pub owner_only_access: bool,
}

/// Resolve the deploy-specific structured model/provider for a managed agent.
#[cfg(test)]
pub(crate) fn resolve_deploy_model_provider(
    record: &ManagedAgentRecord,
    personas: &[AgentDefinition],
    global: &crate::managed_agents::GlobalAgentConfig,
) -> (Option<String>, Option<String>) {
    crate::managed_agents::effective_config::resolve_effective_model_provider_pair(
        record, personas, global,
    )
    .unwrap_or((None, None))
}

/// Serialize the portable launch contract shared with provider-backed agents.
///
/// `descriptor.env` is the authoritative six-layer environment. Policy values
/// are deliberately separate because providers apply them below that layered
/// environment, preserving the local spawn's power-user override semantics.
pub(super) fn build_launch_block(
    record: &ManagedAgentRecord,
    descriptor: &crate::managed_agents::readiness::EffectiveHarnessDescriptor,
    teams: &[crate::managed_agents::TeamRecord],
    effective_prompt: Option<&str>,
    effective_model: Option<&str>,
    owner_pubkey: &str,
) -> serde_json::Value {
    use crate::managed_agents::{
        known_acp_runtime, resolve_session_title, DISPLAY_NAME_ENV_VAR, SESSION_TITLE_ENV_VAR,
    };

    let runtime = known_acp_runtime(&descriptor.command);
    let mut policy_env = BTreeMap::new();

    if let Some(runtime) = runtime {
        policy_env.extend(
            runtime
                .default_env
                .iter()
                .map(|(key, value)| ((*key).to_string(), (*value).to_string())),
        );
        if runtime.mcp_hooks {
            policy_env.insert("MCP_HOOK_SERVERS".into(), "*".into());
        }
    }
    policy_env.insert("BUZZ_ACP_RELAY_OBSERVER".into(), "true".into());
    policy_env.insert("BUZZ_ACP_LAZY_POOL".into(), "true".into());
    policy_env.insert(
        "BUZZ_ACP_AGENTS".into(),
        crate::managed_agents::acp_agents_value(&descriptor.command, record.parallelism),
    );

    if let Some(value) = effective_prompt {
        policy_env.insert("BUZZ_ACP_SYSTEM_PROMPT".into(), value.to_string());
    }
    if let Some(value) = effective_model {
        // B2: remote env-authority model key. Claude's startup model authority
        // is ANTHROPIC_MODEL (same as the local A1 path — the harness reads it
        // first and skips the BUZZ_ACP_MODEL catalog-switch path that would
        // introduce a second startup authority). All other runtimes use
        // BUZZ_ACP_MODEL, which the harness reads into desired_model at spawn.
        let is_claude = runtime.map(|r| r.id == "claude").unwrap_or(false);
        let model_key = if is_claude {
            "ANTHROPIC_MODEL"
        } else {
            "BUZZ_ACP_MODEL"
        };
        policy_env.insert(model_key.into(), value.to_string());
    }
    // I-4: remote parity for persisted startup effort. Mirrors the local spawn
    // path in runtime.rs. The harness reads BUZZ_ACP_EFFORT_LEVEL into
    // PoolStartup.startup_effort and applies it at first session creation via
    // resolve_startup_effort().
    if let Some(ref value) = record.effort_level {
        policy_env.insert("BUZZ_ACP_EFFORT_LEVEL".into(), value.clone());
    }
    if let Some(value) = record.idle_timeout_seconds {
        policy_env.insert("BUZZ_ACP_IDLE_TIMEOUT".into(), value.to_string());
    }
    if let Some(value) = record.max_turn_duration_seconds {
        policy_env.insert("BUZZ_ACP_MAX_TURN_DURATION".into(), value.to_string());
    }
    if let Some(value) = resolve_session_title(record.display_name.as_deref(), &record.name) {
        policy_env.insert(SESSION_TITLE_ENV_VAR.into(), value.clone());
        policy_env.insert(DISPLAY_NAME_ENV_VAR.into(), value);
    }
    if let Some(value) =
        crate::managed_agents::spawn_snapshot::effective_team_instructions(record, teams)
    {
        policy_env.insert("BUZZ_ACP_TEAM_INSTRUCTIONS".into(), value);
    }

    // B5 remote parity: when a canonical effort_level is persisted, strip
    // BUZZ_ACP_EFFORT_LEVEL from launch.env so it cannot shadow the canonical
    // value in policy_env (tier 1). In the k8s three-tier model tier 2
    // (launch.env) overwrites tier 1 (policy_env) — later-wins — so the key
    // must be absent from tier 2 whenever a canonical value is present.
    // When effort_level is None there is no canonical to protect, so user
    // env passthrough stands (env may legitimately seed startup effort).
    //
    // B2 remote parity: mirror the local A1 model authority. For a Claude
    // launch, ALWAYS strip BOTH BUZZ_ACP_MODEL and ANTHROPIC_MODEL from
    // launch.env — the resolved canonical model rides policy_env.ANTHROPIC_MODEL
    // alone (set above), and launch.env later-wins over policy_env. Left in
    // launch.env, a user BUZZ_ACP_MODEL would introduce a second startup
    // authority and a user ANTHROPIC_MODEL would silently override the
    // canonical model. When no canonical model is present, neither key is in
    // policy_env, so stripping them keeps the remote process free of both —
    // matching local, where `apply_claude_model_env(None)` removes both.
    let is_claude = runtime.map(|r| r.id == "claude").unwrap_or(false);
    let strip_key = |k: &str| {
        (record.effort_level.is_some() && k.eq_ignore_ascii_case("BUZZ_ACP_EFFORT_LEVEL"))
            || (is_claude
                && (k.eq_ignore_ascii_case("BUZZ_ACP_MODEL")
                    || k.eq_ignore_ascii_case("ANTHROPIC_MODEL")))
    };
    let launch_env: BTreeMap<String, String> = descriptor
        .env
        .iter()
        .filter(|(k, _)| !strip_key(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    serde_json::json!({
        "command": descriptor.command,
        "args": descriptor.args,
        "env": launch_env,
        "policy_env": policy_env,
        "owner_pubkey": owner_pubkey,
    })
}

pub(super) fn ensure_remote_provider_supported(provider: Option<&str>) -> Result<(), String> {
    if provider.map(str::trim) == Some(crate::managed_agents::RELAY_MESH_PROVIDER_ID) {
        return Err(
            "shared-compute agents cannot be deployed remotely because the mesh endpoint is local to the desktop"
                .to_string(),
        );
    }
    Ok(())
}

/// Inject a wake deploy's replay floor into the launch contract.
///
/// `launch.policy_env` is the provider contract for harness environment
/// (applied below the user env layer). The floor is the `created_at` of the
/// mention that triggered the wake: cold-start latency routinely exceeds the
/// harness's 5s startup replay skew, so without it the fresh harness would
/// subscribe *after* the very message that woke it and never answer it.
/// Transient by design — it lives only in this deploy's payload, never on
/// the record, so ordinary deploys carry no floor.
///
/// The key is in `RESERVED_ENV_KEYS`, so every user-controllable layer
/// (global/persona/agent env) strips it before it can reach `launch.env` —
/// which providers apply AFTER `policy_env` and would otherwise override
/// this value. This function is therefore the only writer.
pub(super) fn apply_wake_replay_floor(
    payload: &mut serde_json::Value,
    wake_replay_floor: Option<u64>,
) {
    if let Some(floor) = wake_replay_floor {
        payload["launch"]["policy_env"]["BUZZ_ACP_REPLAY_FLOOR"] =
            serde_json::Value::String(floor.to_string());
    }
}

/// Build the standard agent JSON payload for provider deploy calls.
///
/// `wake_replay_floor` is set only when this deploy is a wake-on-mention —
/// see [`apply_wake_replay_floor`].
pub(crate) fn build_deploy_payload<R: tauri::Runtime>(
    app: &AppHandle<R>,
    state: &AppState,
    record: &ManagedAgentRecord,
    wake_replay_floor: Option<u64>,
) -> Result<serde_json::Value, String> {
    if let Some(err) = crate::managed_agents::spawn_key_refusal(record) {
        return Err(err);
    }

    let global = crate::managed_agents::load_global_agent_config(app).unwrap_or_default();
    let personas = load_personas(app).unwrap_or_default();
    let teams = crate::managed_agents::load_teams(app).unwrap_or_default();
    let persona_env =
        crate::managed_agents::live_persona_env(&personas, record.persona_id.as_deref());
    let global_persona_env = crate::managed_agents::merged_user_env(&global.env_vars, &persona_env);
    let merged_user_env =
        crate::managed_agents::merged_user_env(&global_persona_env, &record.env_vars);
    let effective = crate::managed_agents::effective_config::resolve_effective_config(
        record, &personas, &global,
    )
    .require_resolved()?;

    ensure_remote_provider_supported(effective.provider.value.as_deref())?;

    let descriptor =
        crate::managed_agents::resolve_effective_harness_descriptor(record, &personas, &global)
            .map_err(|error| crate::managed_agents::user_facing_harness_error(&error))?;
    let owner_pubkey = super::workspace_owner_hex(state)?;
    let launch = build_launch_block(
        record,
        &descriptor,
        &teams,
        effective.system_prompt.value.as_deref(),
        effective.model.value.as_deref(),
        &owner_pubkey,
    );

    let effective_parallelism =
        crate::managed_agents::effective_parallelism(&descriptor.command, record.parallelism);

    let mut payload = deploy_payload_json(
        record,
        crate::relay::effective_agent_relay_url(
            &record.relay_url,
            &relay_ws_url_with_override(state),
        ),
        DeployProjections {
            effective_model: effective.model.value,
            effective_provider: effective.provider.value,
            effective_prompt: effective.system_prompt.value,
            effective_parallelism,
            owner_only_access: crate::managed_agents::owner_only_access_build(),
        },
        merged_user_env,
        launch,
    );
    apply_wake_replay_floor(&mut payload, wake_replay_floor);
    Ok(payload)
}

/// Pure serialization half of [`build_deploy_payload`]. Legacy top-level fields
/// remain for display/bookkeeping; providers execute the resolved `launch` block.
/// `projections.effective_parallelism` is pre-computed from the same resolved
/// descriptor as `launch.policy_env["BUZZ_ACP_AGENTS"]`. Access is projected from
/// the same compiled policy that gates local starts.
pub(super) fn deploy_payload_json(
    record: &ManagedAgentRecord,
    relay_url: String,
    projections: DeployProjections,
    merged_env: BTreeMap<String, String>,
    launch: serde_json::Value,
) -> serde_json::Value {
    let (respond_to, respond_to_allowlist) =
        crate::managed_agents::projected_access_with_policy(record, projections.owner_only_access);
    serde_json::json!({
        "name": &record.name,
        "relay_url": relay_url,
        "private_key_nsec": &record.private_key_nsec,
        "auth_tag": &record.auth_tag,
        "agent_command": &record.agent_command,
        "agent_args": &record.agent_args,
        "system_prompt": projections.effective_prompt,
        "model": projections.effective_model,
        "provider": projections.effective_provider,
        "turn_timeout_seconds": record.turn_timeout_seconds,
        "idle_timeout_seconds": record.idle_timeout_seconds,
        "max_turn_duration_seconds": record.max_turn_duration_seconds,
        // Legacy top-level field: projected from the same resolved descriptor as
        // launch.policy_env["BUZZ_ACP_AGENTS"] — the two are always consistent.
        "parallelism": projections.effective_parallelism,
        "respond_to": respond_to,
        "respond_to_allowlist": respond_to_allowlist,
        "env_vars": merged_env,
        "launch": launch,
    })
}

#[cfg(test)]
mod policy_acknowledgement_tests {
    use super::*;
    use crate::managed_agents::RespondTo;

    fn pending_record() -> ManagedAgentRecord {
        serde_json::from_value(serde_json::json!({
            "pubkey": "agent", "name": "Agent", "relay_url": "", "acp_command": "",
            "agent_command": "", "agent_args": [], "mcp_command": "",
            "turn_timeout_seconds": 0, "system_prompt": null, "created_at": "",
            "updated_at": "", "last_started_at": null, "last_stopped_at": null,
            "last_exit_code": null, "last_error": null,
            "provider_policy_pending": true
        }))
        .unwrap()
    }

    fn policy_payload(respond_to: &str) -> serde_json::Value {
        serde_json::json!({"respond_to": respond_to, "respond_to_allowlist": []})
    }

    fn scoped_payload(relay: &str, owner: &str) -> serde_json::Value {
        serde_json::json!({
            "relay_url": relay,
            "launch": { "owner_pubkey": owner },
        })
    }

    // ── assert_payload_scope: the payload this deploy actually invokes ──────

    #[test]
    fn matching_scope_and_signer_pass_on_the_invoked_payload() {
        assert_payload_scope(
            &scoped_payload("wss://tenant-a.example", "aa11"),
            Some("wss://tenant-a.example"),
            Some("aa11"),
        )
        .unwrap();
    }

    #[test]
    fn relay_switch_during_the_lock_wait_fails_closed() {
        let error = assert_payload_scope(
            &scoped_payload("wss://tenant-b.example", "aa11"),
            Some("wss://tenant-a.example"),
            Some("aa11"),
        )
        .unwrap_err();
        assert!(error.contains("active community changed"), "{error}");
    }

    #[test]
    fn same_relay_identity_switch_during_the_lock_wait_fails_closed() {
        // Same relay, different owner: an identity switch alone must also be
        // refused — the payload's launch.owner_pubkey belongs to a tenant the
        // caller never validated.
        let error = assert_payload_scope(
            &scoped_payload("wss://tenant-a.example", "bb22"),
            Some("wss://tenant-a.example"),
            Some("aa11"),
        )
        .unwrap_err();
        assert!(error.contains("active identity changed"), "{error}");
    }

    #[test]
    fn scoped_caller_with_an_unverifiable_payload_fails_closed() {
        let payload = serde_json::json!({});
        let relay_error =
            assert_payload_scope(&payload, Some("wss://tenant-a.example"), None).unwrap_err();
        assert!(relay_error.contains("no relay"), "{relay_error}");
        let signer_error = assert_payload_scope(&payload, None, Some("aa11")).unwrap_err();
        assert!(signer_error.contains("no owner identity"), "{signer_error}");
    }

    #[test]
    fn unscoped_callers_deploy_any_payload() {
        assert_payload_scope(
            &scoped_payload("wss://anywhere.example", "cc33"),
            None,
            None,
        )
        .unwrap();
        assert_payload_scope(&serde_json::json!({}), None, None).unwrap();
    }

    #[test]
    fn successful_deploy_acknowledges_pending_policy() {
        let mut record = pending_record();

        acknowledge_policy(&mut record, &policy_payload("owner-only"));

        assert!(!record.provider_policy_pending);
    }

    /// A deploy carrying a policy the record has since moved off must leave the
    /// pending flag up so the reconcile retries with the current one.
    #[test]
    fn successful_stale_deploy_preserves_newer_pending_policy() {
        let mut record = pending_record();
        record.respond_to = RespondTo::Anyone;

        acknowledge_policy(&mut record, &policy_payload("owner-only"));

        assert!(record.provider_policy_pending);
    }

    #[test]
    fn allowlist_drift_preserves_pending_policy() {
        let mut record = pending_record();
        record.respond_to_allowlist = vec!["someone".to_string()];

        acknowledge_policy(&mut record, &policy_payload("owner-only"));

        assert!(record.provider_policy_pending);
    }
}

#[cfg(test)]
#[path = "agents_deploy_tests.rs"]
mod tests;
