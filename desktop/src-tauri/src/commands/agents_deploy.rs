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
pub(crate) async fn deploy_to_provider(
    app: &AppHandle,
    state: &AppState,
    pubkey: &str,
    provider_id: &str,
    config: &serde_json::Value,
    agent_json: serde_json::Value,
    cached_binary_path: Option<&str>,
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
        policy_env.insert("BUZZ_ACP_MODEL".into(), value.to_string());
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

    serde_json::json!({
        "command": descriptor.command,
        "args": descriptor.args,
        "env": descriptor.env,
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
pub(crate) fn build_deploy_payload(
    app: &AppHandle,
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
mod tests {
    use super::*;
    use crate::managed_agents::{readiness::EffectiveHarnessDescriptor, RespondTo, TeamRecord};

    fn record() -> ManagedAgentRecord {
        serde_json::from_value(serde_json::json!({
            "pubkey": "abcd1234",
            "name": "agent-handle",
            "display_name": "Agent\u{0000} Name",
            "private_key_nsec": "nsec1fake",
            "relay_url": "wss://relay.example",
            "acp_command": "buzz-acp",
            "agent_command": "goose",
            "agent_args": [],
            "mcp_command": "",
            "turn_timeout_seconds": 320,
            "idle_timeout_seconds": 17,
            "max_turn_duration_seconds": 23,
            "parallelism": 4,
            "respond_to": RespondTo::OwnerOnly,
            "respond_to_allowlist": [],
            "team_id": "team-1",
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        }))
        .unwrap()
    }

    #[test]
    fn launch_block_preserves_descriptor_and_spawn_policy() {
        let record = record();
        let descriptor = EffectiveHarnessDescriptor {
            command: "goose".into(),
            args: vec!["acp".into()],
            env: BTreeMap::from([
                ("GOOSE_MODE".into(), "custom".into()),
                ("SECRET_FROM_PERSONA".into(), "secret".into()),
            ]),
        };
        let teams: Vec<TeamRecord> = serde_json::from_value(serde_json::json!([{
            "id": "team-1", "name": "Team", "instructions": "Coordinate", "persona_ids": [], "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
        }])).unwrap();

        let launch = build_launch_block(
            &record,
            &descriptor,
            &teams,
            Some("prompt"),
            Some("model"),
            "owner-hex",
        );

        assert_eq!(launch["command"], "goose");
        assert_eq!(launch["args"], serde_json::json!(["acp"]));
        assert_eq!(launch["env"]["GOOSE_MODE"], "custom");
        // policy_env is applied first, so this default remains separate from
        // the descriptor value that wins in launch.env.
        assert_eq!(launch["policy_env"]["GOOSE_MODE"], "auto");
        assert_eq!(launch["policy_env"]["BUZZ_ACP_LAZY_POOL"], "true");
        assert_eq!(launch["policy_env"]["BUZZ_ACP_RELAY_OBSERVER"], "true");
        assert_eq!(
            launch["policy_env"]["BUZZ_ACP_TEAM_INSTRUCTIONS"],
            "Coordinate"
        );
        assert_eq!(launch["policy_env"]["BUZZ_ACP_SESSION_TITLE"], "Agent Name");
        assert_eq!(launch["policy_env"]["BUZZ_ACP_DISPLAY_NAME"], "Agent Name");
        assert_eq!(launch["policy_env"]["BUZZ_ACP_SYSTEM_PROMPT"], "prompt");
        assert_eq!(launch["policy_env"]["BUZZ_ACP_MODEL"], "model");
        assert_eq!(launch["policy_env"]["BUZZ_ACP_IDLE_TIMEOUT"], "17");
        assert_eq!(launch["policy_env"]["BUZZ_ACP_MAX_TURN_DURATION"], "23");
        assert_eq!(launch["policy_env"]["BUZZ_ACP_AGENTS"], "4");
        assert_eq!(launch["owner_pubkey"], "owner-hex");
    }

    /// OpenClaw descriptor: `launch.policy_env["BUZZ_ACP_AGENTS"]` must be "5"
    /// even when the record's requested parallelism is 10. This is the direct
    /// `launch.policy_env` seam test — the executable contract for remote providers.
    #[test]
    fn launch_block_openclaw_over_cap_policy_env_is_capped() {
        let mut record = record();
        record.agent_command = "openclaw".into();
        record.parallelism = 10; // above the OpenClaw spawn-time cap
        let descriptor = EffectiveHarnessDescriptor {
            command: "openclaw".into(),
            args: vec![],
            env: BTreeMap::new(),
        };

        let launch = build_launch_block(&record, &descriptor, &[], None, None, "owner-hex");

        assert_eq!(
            launch["policy_env"]["BUZZ_ACP_AGENTS"],
            crate::managed_agents::parallelism::OPENCLAW_MAX_PARALLELISM.to_string(),
            "launch.policy_env[BUZZ_ACP_AGENTS] must be capped at {} for OpenClaw, not 10",
            crate::managed_agents::parallelism::OPENCLAW_MAX_PARALLELISM
        );
    }

    /// Uncapped harness (goose): `launch.policy_env["BUZZ_ACP_AGENTS"]` passes
    /// the requested value through unchanged.
    #[test]
    fn launch_block_goose_policy_env_is_not_capped() {
        let mut record = record();
        record.parallelism = 8;
        let descriptor = EffectiveHarnessDescriptor {
            command: "goose".into(),
            args: vec![],
            env: BTreeMap::new(),
        };

        let launch = build_launch_block(&record, &descriptor, &[], None, None, "owner-hex");

        assert_eq!(
            launch["policy_env"]["BUZZ_ACP_AGENTS"], "8",
            "goose: policy_env[BUZZ_ACP_AGENTS] must pass through requested value 8"
        );
    }

    /// deploy_payload_json: legacy top-level `parallelism` is the effective value
    /// derived from the descriptor, not `record.agent_command`.
    ///
    /// Stale-persona scenario: `record.agent_command` is "goose" (created before
    /// the user switched the persona to OpenClaw), but the live descriptor resolves
    /// OpenClaw. Both `launch.policy_env["BUZZ_ACP_AGENTS"]` and the legacy
    /// top-level `parallelism` must be the effective OpenClaw value (5), not the
    /// record's stale Goose identity (requested 10).
    #[test]
    fn deploy_payload_json_stale_goose_record_live_openclaw_descriptor_both_capped() {
        let mut record = record();
        // Stale agent_command from record creation — persona has since switched to OpenClaw.
        record.agent_command = "goose".into();
        record.parallelism = 10;
        // Resolved descriptor reflects the live persona (OpenClaw).
        let descriptor = EffectiveHarnessDescriptor {
            command: "openclaw".into(),
            args: vec![],
            env: BTreeMap::new(),
        };
        let cap = crate::managed_agents::parallelism::OPENCLAW_MAX_PARALLELISM;

        let launch = build_launch_block(&record, &descriptor, &[], None, None, "owner-hex");
        let effective_parallelism =
            crate::managed_agents::effective_parallelism(&descriptor.command, record.parallelism);
        let payload = deploy_payload_json(
            &record,
            "wss://relay.example".to_string(),
            DeployProjections {
                effective_model: None,
                effective_provider: None,
                effective_prompt: None,
                effective_parallelism,
                owner_only_access: false,
            },
            BTreeMap::new(),
            launch.clone(),
        );

        assert_eq!(
            launch["policy_env"]["BUZZ_ACP_AGENTS"],
            cap.to_string(),
            "launch.policy_env[BUZZ_ACP_AGENTS] must be capped at {cap} for live OpenClaw descriptor"
        );
        assert_eq!(
            payload["parallelism"], cap,
            "legacy top-level parallelism must match launch.policy_env — both must be {cap}"
        );
    }

    /// Inverse stale-persona scenario: `record.agent_command` is "openclaw"
    /// (created before the user switched the persona to Goose), but the live
    /// descriptor resolves Goose. Both projections must be the uncapped requested
    /// value (4), not the old OpenClaw cap.
    #[test]
    fn deploy_payload_json_stale_openclaw_record_live_goose_descriptor_both_uncapped() {
        let mut record = record();
        // Stale agent_command from record creation — persona has since switched to Goose.
        record.agent_command = "openclaw".into();
        record.parallelism = 4;
        // Resolved descriptor reflects the live persona (Goose).
        let descriptor = EffectiveHarnessDescriptor {
            command: "goose".into(),
            args: vec![],
            env: BTreeMap::new(),
        };

        let launch = build_launch_block(&record, &descriptor, &[], None, None, "owner-hex");
        let effective_parallelism =
            crate::managed_agents::effective_parallelism(&descriptor.command, record.parallelism);
        let payload = deploy_payload_json(
            &record,
            "wss://relay.example".to_string(),
            DeployProjections {
                effective_model: None,
                effective_provider: None,
                effective_prompt: None,
                effective_parallelism,
                owner_only_access: false,
            },
            BTreeMap::new(),
            launch.clone(),
        );

        assert_eq!(
            launch["policy_env"]["BUZZ_ACP_AGENTS"],
            "4",
            "launch.policy_env[BUZZ_ACP_AGENTS] must pass through requested 4 for live Goose descriptor"
        );
        assert_eq!(
            payload["parallelism"], 4,
            "legacy top-level parallelism must match launch.policy_env — both must be 4 (uncapped)"
        );
    }

    /// Explicit agent_command_override direction: record has an explicit override
    /// pinning OpenClaw while the persona default is Goose. The override wins
    /// via the descriptor — both projections must be capped at the OpenClaw limit.
    #[test]
    fn deploy_payload_json_explicit_openclaw_override_both_capped() {
        let mut record = record();
        // Explicit override: user pinned OpenClaw on this agent.
        record.agent_command_override = Some("openclaw".into());
        record.agent_command = "goose".into(); // persona default, overridden
        record.parallelism = 10;
        // Descriptor reflects the resolved override (OpenClaw wins).
        let descriptor = EffectiveHarnessDescriptor {
            command: "openclaw".into(),
            args: vec![],
            env: BTreeMap::new(),
        };
        let cap = crate::managed_agents::parallelism::OPENCLAW_MAX_PARALLELISM;

        let launch = build_launch_block(&record, &descriptor, &[], None, None, "owner-hex");
        let effective_parallelism =
            crate::managed_agents::effective_parallelism(&descriptor.command, record.parallelism);
        let payload = deploy_payload_json(
            &record,
            "wss://relay.example".to_string(),
            DeployProjections {
                effective_model: None,
                effective_provider: None,
                effective_prompt: None,
                effective_parallelism,
                owner_only_access: false,
            },
            BTreeMap::new(),
            launch.clone(),
        );

        assert_eq!(
            launch["policy_env"]["BUZZ_ACP_AGENTS"],
            cap.to_string(),
            "launch.policy_env[BUZZ_ACP_AGENTS] must be {cap} for explicit OpenClaw override"
        );
        assert_eq!(
            payload["parallelism"], cap,
            "legacy top-level parallelism must match launch.policy_env — both must be {cap}"
        );
    }

    /// A wake deploy injects its trigger timestamp into `launch.policy_env`;
    /// an ordinary deploy leaves the launch contract untouched.
    #[test]
    fn wake_replay_floor_is_injected_only_when_present() {
        let record = record();
        let descriptor = EffectiveHarnessDescriptor {
            command: "goose".into(),
            args: vec![],
            env: BTreeMap::new(),
        };
        let launch = build_launch_block(&record, &descriptor, &[], None, None, "owner-hex");
        let mut payload = serde_json::json!({ "launch": launch });

        apply_wake_replay_floor(&mut payload, None);
        assert!(
            payload["launch"]["policy_env"]
                .get("BUZZ_ACP_REPLAY_FLOOR")
                .is_none(),
            "ordinary deploys must not carry a replay floor"
        );

        apply_wake_replay_floor(&mut payload, Some(1_700_000_123));
        assert_eq!(
            payload["launch"]["policy_env"]["BUZZ_ACP_REPLAY_FLOOR"], "1700000123",
            "wake deploys must carry the trigger timestamp as a string env value"
        );
    }
}
