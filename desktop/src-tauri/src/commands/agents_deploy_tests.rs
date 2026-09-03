//! Unit tests for `agents_deploy`. Included via
//! `#[path = "agents_deploy_tests.rs"] mod tests;` at the bottom of
//! `agents_deploy.rs`, which keeps that module under the file-size ratchet.

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
    // goose runtime: model goes via BUZZ_ACP_MODEL (non-claude path).
    assert_eq!(launch["policy_env"]["BUZZ_ACP_MODEL"], "model");
    assert!(
        launch["policy_env"]["ANTHROPIC_MODEL"].is_null(),
        "goose must NOT receive ANTHROPIC_MODEL"
    );
    assert_eq!(launch["policy_env"]["BUZZ_ACP_IDLE_TIMEOUT"], "17");
    assert_eq!(launch["policy_env"]["BUZZ_ACP_MAX_TURN_DURATION"], "23");
    assert_eq!(launch["policy_env"]["BUZZ_ACP_AGENTS"], "4");
    assert_eq!(launch["policy_env"]["BUZZ_ACP_SESSION_POLICY"], "channel");
    assert_eq!(launch["owner_pubkey"], "owner-hex");
}

#[test]
fn launch_block_thread_policy_is_authoritative_and_preserves_unrelated_env() {
    let record = record();
    let descriptor = EffectiveHarnessDescriptor {
        command: "goose".into(),
        args: vec![],
        env: BTreeMap::from([
            ("BUZZ_ACP_SESSION_POLICY".to_string(), "channel".to_string()),
            ("KEEP_ME".to_string(), "yes".to_string()),
        ]),
    };

    let launch = build_launch_block_for_policy(
        &record,
        &descriptor,
        &[],
        None,
        None,
        "owner-hex",
        crate::managed_agents::AcpSessionPolicy::Thread,
    );

    assert_eq!(launch["policy_env"]["BUZZ_ACP_SESSION_POLICY"], "thread");
    assert!(
        launch["env"]["BUZZ_ACP_SESSION_POLICY"].is_null(),
        "desktop policy must not be shadowed by descriptor env"
    );
    assert_eq!(launch["env"]["KEEP_ME"], "yes");
}

#[test]
fn launch_block_claude_runtime_uses_anthropic_model_not_buzz_acp_model() {
    // B2: remote claude deploys must send ANTHROPIC_MODEL, not BUZZ_ACP_MODEL,
    // so the remote harness has a single startup model authority matching A1.
    let record = record();
    let descriptor = EffectiveHarnessDescriptor {
        command: "claude".into(),
        args: vec![],
        env: BTreeMap::new(),
    };
    let teams: Vec<TeamRecord> = vec![];
    let launch = build_launch_block(
        &record,
        &descriptor,
        &teams,
        None,
        Some("claude-opus-4"),
        "owner-hex",
    );
    assert_eq!(
        launch["policy_env"]["ANTHROPIC_MODEL"], "claude-opus-4",
        "claude remote must receive ANTHROPIC_MODEL"
    );
    assert!(
        launch["policy_env"]["BUZZ_ACP_MODEL"].is_null(),
        "claude remote must NOT receive BUZZ_ACP_MODEL"
    );
}

/// F2: remote Claude launch must mirror local A1 — ALWAYS strip BOTH
/// BUZZ_ACP_MODEL and ANTHROPIC_MODEL from launch.env (tier 2), so the
/// canonical model in policy_env (tier 1) is the sole authority. Since
/// launch.env later-wins over policy_env, a user BUZZ_ACP_MODEL would add a
/// second startup authority and a user ANTHROPIC_MODEL would silently
/// override the canonical model.
#[test]
fn launch_block_claude_strips_both_model_keys_from_launch_env() {
    let record = record();
    let descriptor = EffectiveHarnessDescriptor {
        command: "claude".into(),
        args: vec![],
        env: BTreeMap::from([
            ("BUZZ_ACP_MODEL".to_string(), "user-sonnet".to_string()),
            ("ANTHROPIC_MODEL".to_string(), "user-opus".to_string()),
            ("KEEP_ME".to_string(), "yes".to_string()),
        ]),
    };
    let launch = build_launch_block(
        &record,
        &descriptor,
        &[],
        None,
        Some("claude-opus-4"),
        "owner-hex",
    );

    // Canonical model rides policy_env alone.
    assert_eq!(launch["policy_env"]["ANTHROPIC_MODEL"], "claude-opus-4");
    assert!(launch["policy_env"]["BUZZ_ACP_MODEL"].is_null());
    // Both model keys are stripped from launch.env — neither can later-win.
    assert!(
        launch["env"]["BUZZ_ACP_MODEL"].is_null(),
        "user BUZZ_ACP_MODEL must be stripped from launch.env for claude"
    );
    assert!(
        launch["env"]["ANTHROPIC_MODEL"].is_null(),
        "user ANTHROPIC_MODEL must be stripped from launch.env for claude"
    );
    // Unrelated user env survives.
    assert_eq!(launch["env"]["KEEP_ME"], "yes");
}

/// F2: when no canonical model resolves, a Claude launch still strips both
/// model keys from launch.env, so neither authority reaches the remote
/// process — matching local `apply_claude_model_env(None)`, which removes
/// both.
#[test]
fn launch_block_claude_strips_model_keys_even_without_canonical() {
    let record = record();
    let descriptor = EffectiveHarnessDescriptor {
        command: "claude".into(),
        args: vec![],
        env: BTreeMap::from([
            ("BUZZ_ACP_MODEL".to_string(), "user-sonnet".to_string()),
            ("ANTHROPIC_MODEL".to_string(), "user-opus".to_string()),
        ]),
    };
    let launch = build_launch_block(&record, &descriptor, &[], None, None, "owner-hex");

    assert!(launch["policy_env"]["ANTHROPIC_MODEL"].is_null());
    assert!(launch["policy_env"]["BUZZ_ACP_MODEL"].is_null());
    assert!(
        launch["env"]["BUZZ_ACP_MODEL"].is_null(),
        "user BUZZ_ACP_MODEL must be stripped even without a canonical model"
    );
    assert!(
        launch["env"]["ANTHROPIC_MODEL"].is_null(),
        "user ANTHROPIC_MODEL must be stripped even without a canonical model"
    );
}

/// F2: non-Claude runtimes must NOT strip model keys from launch.env — the
/// model authority stripping is Claude-specific (BUZZ_ACP_MODEL is the
/// spawn authority for other runtimes and rides policy_env there).
#[test]
fn launch_block_non_claude_preserves_user_model_env() {
    let record = record(); // goose command
    let descriptor = EffectiveHarnessDescriptor {
        command: "goose".into(),
        args: vec![],
        env: BTreeMap::from([("BUZZ_ACP_MODEL".to_string(), "user-model".to_string())]),
    };
    let launch = build_launch_block(&record, &descriptor, &[], None, Some("model"), "owner-hex");

    // goose puts canonical in policy_env, and the user launch.env value is
    // preserved (later-wins is the intended goose behavior).
    assert_eq!(launch["policy_env"]["BUZZ_ACP_MODEL"], "model");
    assert_eq!(launch["env"]["BUZZ_ACP_MODEL"], "user-model");
}

#[test]
fn launch_block_claude_runtime_carries_projected_effort_in_launch_env() {
    // Under the harness-agnostic projection, effort no longer rides
    // policy_env: `resolve_effective_harness_descriptor` reduces
    // `descriptor.env` to exactly one effort key (for a keyless claude
    // runtime, the ACP sentinel) holding the effective value, and
    // build_launch_block passes that env through to launch.env verbatim.
    let record = record();
    let descriptor = EffectiveHarnessDescriptor {
        command: "claude".into(),
        args: vec![],
        // The single projected effort key the descriptor resolver emits.
        env: BTreeMap::from([("BUZZ_ACP_EFFORT_LEVEL".to_string(), "high".to_string())]),
    };
    let launch = build_launch_block(&record, &descriptor, &[], None, None, "owner-hex");
    assert_eq!(
        launch["env"]["BUZZ_ACP_EFFORT_LEVEL"], "high",
        "the projected effort key must survive into launch.env"
    );
    assert!(
        launch["policy_env"]["BUZZ_ACP_EFFORT_LEVEL"].is_null(),
        "effort is not a policy_env value under the projection design"
    );
}

#[test]
fn launch_block_does_not_inject_effort_level_when_absent() {
    // I-4: no BUZZ_ACP_EFFORT_LEVEL in policy_env when record.effort_level is None.
    let record = record(); // effort_level is None by default
    let descriptor = EffectiveHarnessDescriptor {
        command: "claude".into(),
        args: vec![],
        env: BTreeMap::new(),
    };
    let launch = build_launch_block(&record, &descriptor, &[], None, None, "owner-hex");
    assert!(
        launch["policy_env"]["BUZZ_ACP_EFFORT_LEVEL"].is_null(),
        "policy_env must NOT contain BUZZ_ACP_EFFORT_LEVEL when effort_level is None"
    );
}

/// B5 remote parity: when a canonical effort_level is persisted, a conflicting
/// user-supplied BUZZ_ACP_EFFORT_LEVEL in descriptor.env must NOT shadow it.
/// The projection resolves the collision before build_launch_block sees it, so
/// the canonical value is the single effort key carried into launch.env.
#[test]
fn launch_block_canonical_effort_strips_user_env_collision() {
    // Remote parity for the authority collision: the canonical column and a
    // conflicting user `BUZZ_ACP_EFFORT_LEVEL` both present. The projection
    // (run inside `resolve_effective_harness_descriptor`) resolves it —
    // canonical `high` wins over the user `low` transport sentinel — and
    // build_launch_block carries exactly that one value into launch.env,
    // identical to the local spawn path.
    let mut record = record();
    record.runtime = Some("claude".into());
    record.effort_level = Some("high".to_string());
    record
        .env_vars
        .insert("BUZZ_ACP_EFFORT_LEVEL".into(), "low".into());
    let descriptor = crate::managed_agents::resolve_effective_harness_descriptor(
        &record,
        &[],
        &Default::default(),
    )
    .expect("claude descriptor resolves");
    let launch = build_launch_block(&record, &descriptor, &[], None, None, "owner-hex");

    // The projected canonical authority is the single effort value carried.
    assert_eq!(
        launch["env"]["BUZZ_ACP_EFFORT_LEVEL"], "high",
        "canonical effort must win the collision and reach launch.env"
    );
    // Effort is not a policy_env value under the projection design.
    assert!(
        launch["policy_env"]["BUZZ_ACP_EFFORT_LEVEL"].is_null(),
        "effort is carried in launch.env, never policy_env"
    );
}

/// B5 remote parity: when no canonical effort is persisted (effort_level is
/// None), a user-supplied BUZZ_ACP_EFFORT_LEVEL in descriptor.env survives
/// into launch.env — passthrough preserved for startup seeding.
#[test]
fn launch_block_user_effort_env_survives_when_no_canonical_value() {
    let record = record(); // effort_level is None
    let descriptor = EffectiveHarnessDescriptor {
        command: "claude".into(),
        args: vec![],
        env: BTreeMap::from([("BUZZ_ACP_EFFORT_LEVEL".to_string(), "low".to_string())]),
    };
    let launch = build_launch_block(&record, &descriptor, &[], None, None, "owner-hex");

    // No canonical — key must NOT appear in policy_env.
    assert!(
        launch["policy_env"]["BUZZ_ACP_EFFORT_LEVEL"].is_null(),
        "policy_env must NOT contain BUZZ_ACP_EFFORT_LEVEL when effort_level is None"
    );
    // User value must survive in launch.env so the harness can use it.
    assert_eq!(
        launch["env"]["BUZZ_ACP_EFFORT_LEVEL"], "low",
        "user-supplied effort must survive in launch.env when no canonical value"
    );
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
