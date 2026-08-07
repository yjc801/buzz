//! Building the agent environment (spec §Launch data, §Entrypoint mapping
//! table) and serializing it for the launcher.
//!
//! The three tiers are resolved *here*, before serialization, because the
//! env file the launcher sources is a flat sequence of `export` lines with no
//! precedence of its own: if two tiers supplied the same key, whichever line
//! landed last would win silently. Resolving in-provider makes later-wins
//! explicit and testable.
//!
//! Transport (this binding): the resolved map is serialized to `export`
//! lines and streamed over exec-WebSocket **stdin frames** into a
//! `/dev/shm` file the launcher sources and deletes before `exec`ing the
//! harness. Never the exec URL's `env=` query parameter (URL-shaped values
//! reach access logs), never the sprite's API-readable `environment` map,
//! never the durable filesystem (it is object-storage-synced and
//! checkpointable).

use crate::wire::{AgentPayload, LaunchBlock};
use std::collections::BTreeMap;

/// Keys the authoritative tier owns.
///
/// Load-bearing, not documentation: tier 3 *clears* every key on this list
/// before writing its own values, so a key the authoritative tier has no value
/// for is **removed** rather than left holding a lower-tier value. Plain
/// overwrite is not enough — most of these are written conditionally
/// (`BUZZ_ACP_AGENT_ARGS` only when `launch.args` is non-empty,
/// `BUZZ_ACP_RESPOND_TO` only when set), and without the clear, a lower tier
/// could supply the value for exactly the cases the authoritative tier stays
/// silent on. Clearing is also what the local spawn does: the desktop strips
/// reserved keys from user env before the authoritative layer is written, so
/// absent-means-absent in both paths.
const AUTHORITATIVE_KEYS: &[&str] = &[
    "BUZZ_RELAY_URL",
    "BUZZ_PRIVATE_KEY",
    "NOSTR_PRIVATE_KEY",
    "BUZZ_AUTH_TAG",
    "BUZZ_ACP_AGENT_OWNER",
    "BUZZ_ACP_AGENT_COMMAND",
    "BUZZ_ACP_AGENT_ARGS",
    "BUZZ_ACP_RESPOND_TO",
    "BUZZ_ACP_RESPOND_TO_ALLOWLIST",
    "BUZZ_ACP_MCP_COMMAND",
    "BUZZ_ACP_EXIT_AFTER_INACTIVITY",
    START_NONCE_KEY,
];

/// The attempt's generation, as the harness sees it. Also the per-attempt env
/// file's name suffix — one generation, one identity — so the reconciler
/// restamps this on every start attempt rather than letting a value persist
/// across a retry.
pub const START_NONCE_KEY: &str = "BUZZ_MANAGED_AGENT_START_NONCE";

/// Presence is the only remote liveness signal (I3), so a launch that
/// suppresses it is non-conforming (L1 item 2) — and unlike a reserved-key
/// collision, there is no "authoritative value" to overwrite it with. Refuse.
const FORBIDDEN_KEY: &str = "BUZZ_ACP_NO_PRESENCE";

/// Bound on the summed value bytes of the serialized environment. The env
/// file is streamed into a 64MiB tmpfs (`/dev/shm`); an environment past this
/// bound is a mistake, not a workload, and surfaces as a named provider error
/// rather than a half-written file inside the sprite.
const MAX_ENV_BYTES: usize = 1024 * 1024;

/// A POSIX-shaped env var name: `[A-Za-z_][A-Za-z0-9_]*`.
///
/// The environment reaches the harness as `export` lines sourced by a bash
/// launcher, and bash cannot `export` a name outside this grammar — a looser
/// key would fail inside the sprite, after the deploy already reported
/// progress. Fail closed here instead, with the key named.
fn is_posix_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c == '_' || c.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

/// An identity component (L1 item 1) is present only if it is nonempty after
/// trimming — and the **trimmed form is what gets stored**. The validator and
/// the writer must never disagree about the value: a guard that accepts
/// `"  wss://relay  "` and then writes it with the padding intact has only
/// moved the failure from a loud refusal to a connect error in the harness.
fn identity_component(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

/// The Codex adapter's launch command, as the launch gate admits it
/// (`provision::require_provisioned_command`) and as provisioning installs
/// its npm bin.
const CODEX_ADAPTER_COMMAND: &str = "codex-acp";

/// The `INITIAL_AGENT_MODE` values the pinned codex-acp parses, mapped from
/// `provider_config.preapprove_agent_tools`. Codex never reads Claude Code's
/// settings file — its tool policy is its *mode*, read from this variable at
/// startup, defaulting to workspace-write "agent" mode. So the flag must be
/// realized here or it silently does not apply to Codex agents: false would
/// still edit files and run commands, and true would still leave escalated
/// operations asking a harness that denies every request. On maps to full
/// access (approvals never — the Claude realization already grants shell and
/// network); off maps to read-only, where every edit/exec asks and the
/// harness's denial makes the agent effectively converse-only.
const CODEX_MODE_FULL_ACCESS: &str = "agent-full-access";
const CODEX_MODE_READ_ONLY: &str = "read-only";

/// The harness's `allowlist` gate mode, spelled as the desktop serializes
/// `RespondTo` (kebab-case) and as `buzz-acp`'s CLI parses it.
const RESPOND_TO_ALLOWLIST: &str = "allowlist";

/// Every gate mode `buzz-acp` accepts, spelled as its `clap::ValueEnum`
/// parses them (kebab-case).
///
/// Deliberately the **harness's** four and not the desktop's three: the
/// desktop rejects `nobody` on purpose, but the harness starts fine with it.
/// This guard exists to cover non-desktop callers, so inheriting a
/// desktop-only narrowing would refuse a launch that works.
const RESPOND_TO_MODES: [&str; 4] = ["owner-only", RESPOND_TO_ALLOWLIST, "anyone", "nobody"];

/// Refuse a respond-to gate the harness will reject at config parse.
///
/// Without this, a gate the harness refuses becomes an agent session that
/// exits at startup; nothing restarts it (by design), so the user-visible
/// ending is "startup not confirmed" on every attempt — indistinguishable
/// from a slow platform.
///
/// Mirrors `buzz-acp`'s own rules exactly, deliberately including their
/// asymmetry: the allowlist is validated **only** in allowlist mode, and
/// merely warned about otherwise. Validating it in every mode would refuse a
/// deploy whose identical local spawn succeeds — and a stale list is already
/// harmless here, since `BUZZ_ACP_RESPOND_TO_ALLOWLIST` is an authoritative
/// key that tier 3 clears.
fn validate_respond_to_gate(respond_to: &str, allowlist: Option<&[String]>) -> Result<(), String> {
    // Exact, untrimmed: `clap` does not trim, so `" allowlist "` is `rc=2` at
    // the harness — a parse failure even earlier than the config errors below.
    if !RESPOND_TO_MODES.contains(&respond_to) {
        return Err(format!(
            "deploy refused: respond_to {respond_to:?} is not a mode the \
             harness accepts (expected one of {}) — the agent session would \
             fail to parse its arguments at startup on every attempt",
            RESPOND_TO_MODES.join(", ")
        ));
    }
    if respond_to != RESPOND_TO_ALLOWLIST {
        return Ok(());
    }
    let entries = allowlist.unwrap_or_default();
    if entries.is_empty() {
        return Err(format!(
            "deploy refused: respond_to is {RESPOND_TO_ALLOWLIST:?} but the \
             allowlist is empty — the harness refuses this at startup, so the \
             agent session would fail on every attempt"
        ));
    }
    for entry in entries {
        let trimmed = entry.trim();
        if trimmed.len() != 64 || !trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!(
                "deploy refused: invalid pubkey in respond_to_allowlist: \
                 {entry:?} (must be exactly 64 hex characters)"
            ));
        }
    }
    Ok(())
}

/// Inputs the provider itself supplies to the authoritative tier.
pub struct AuthoritativeInputs<'a> {
    /// The attempt's generation token — also the env file's name suffix, so
    /// the lifecycle correlator and the attempt are one identity.
    pub generation: &'a str,
    /// Resolved from `provider_config.inactivity_seconds`; `None` when the
    /// indefinite opt-in was chosen (which this version refuses elsewhere).
    pub inactivity_seconds: Option<u64>,
    /// `provider_config.preapprove_agent_tools`. The Claude realization is
    /// provisioned state (settings allow rules); the Codex realization is
    /// environment — `INITIAL_AGENT_MODE`, written as a tier-1 default so
    /// an explicit value in policy or user env wins.
    pub preapprove_agent_tools: bool,
}

/// Resolve the full agent environment.
///
/// Order is the spec's, and the function body is deliberately three writes in
/// that order — tier 1, tier 2, tier 3 — so "later wins" is visible rather
/// than argued.
pub fn build_env(
    agent: &AgentPayload,
    auth: AuthoritativeInputs<'_>,
) -> Result<BTreeMap<String, String>, String> {
    let default_launch = LaunchBlock::default();
    let launch = agent.launch.as_ref().unwrap_or(&default_launch);

    let mut env: BTreeMap<String, String> = BTreeMap::new();

    // Tier 1 — overridable behavior defaults. The provider's own default —
    // the Codex realization of `preapprove_agent_tools` (see
    // [`CODEX_MODE_FULL_ACCESS`]) — is written first, so both the desktop's
    // policy env and the user's env can override it.
    if launch.command.as_deref().map(str::trim) == Some(CODEX_ADAPTER_COMMAND) {
        env.insert(
            "INITIAL_AGENT_MODE".into(),
            if auth.preapprove_agent_tools {
                CODEX_MODE_FULL_ACCESS
            } else {
                CODEX_MODE_READ_ONLY
            }
            .into(),
        );
    }
    env.extend(launch.policy_env.clone());

    // Tier 2 — user/layered env. The descriptor already merged
    // global < persona < agent, so `agent.env_vars` is NOT re-merged on top
    // (§Launch data tier 2) — doing so would resurrect a layer the desktop
    // already resolved. When the desktop predates the `launch` block we fall
    // back to the legacy field, which is the only case it is the truth.
    if agent.launch.is_some() {
        env.extend(launch.env.clone());
    } else {
        env.extend(agent.env_vars.clone());
    }

    // Validate what the lower tiers contributed, before the authoritative
    // tier overwrites any of it. A reserved-key collision is NOT fatal: the
    // spec's precedence is later-wins, so tier 3 simply overwrites it, which
    // is exactly what a local spawn does. Only a key that has no
    // authoritative counterpart to overwrite it — presence suppression — is
    // a refusal.
    for key in env.keys() {
        if !is_posix_env_key(key) {
            return Err(format!(
                "env key {key:?} is not a POSIX environment variable name \
                 ([A-Za-z_][A-Za-z0-9_]*); the launcher's shell could not \
                 export it"
            ));
        }
        if key.eq_ignore_ascii_case(FORBIDDEN_KEY) {
            return Err(format!(
                "{FORBIDDEN_KEY} must not be set on a remote agent: presence \
                 is the only signal that a remote agent is alive"
            ));
        }
    }

    // Tier 3 — authoritative. Every key it owns is cleared first, then the
    // values it has are written, so it wins at a key whether or not it has a
    // value there (see [`AUTHORITATIVE_KEYS`]).
    for key in AUTHORITATIVE_KEYS {
        env.remove(*key);
    }
    // Identity comes from top-level payload fields, never from `env_vars`
    // (§Reserved-key rule). All three components must be nonempty: an agent
    // that cannot reach a relay is the identityless launch L1 item 1 exists
    // to prevent, and a blank field would otherwise sail through into the
    // env file and produce a session that starts, fails to connect, and
    // looks like a network problem.
    let Some(relay_url) = identity_component(&agent.relay_url) else {
        return Err("deploy refused: relay_url is empty — the agent would have \
                    no relay to connect to"
            .to_string());
    };
    env.insert("BUZZ_RELAY_URL".into(), relay_url.to_string());
    env.insert("BUZZ_PRIVATE_KEY".into(), agent.private_key_nsec.clone());
    // The git credential/signing helpers read NOSTR_PRIVATE_KEY.
    env.insert("NOSTR_PRIVATE_KEY".into(), agent.private_key_nsec.clone());

    // Owner: at least one of these must resolve, or the harness cannot match
    // `!shutdown` and §Stop describes a mechanism that does not work.
    let auth_tag = agent.auth_tag.as_deref().and_then(identity_component);
    let owner = launch.owner_pubkey.as_deref().and_then(identity_component);
    match (auth_tag, owner) {
        (None, None) => {
            return Err("deploy refused: neither auth_tag nor launch.owner_pubkey \
                        resolved — without an owner the agent cannot honor \
                        !shutdown"
                .to_string())
        }
        (tag, own) => {
            if let Some(t) = tag {
                env.insert("BUZZ_AUTH_TAG".into(), t.to_string());
            }
            if let Some(o) = own {
                env.insert("BUZZ_ACP_AGENT_OWNER".into(), o.to_string());
            }
        }
    }

    // The harness and MCP binaries are resolved against the *provisioned*
    // PATH inside the sprite. A host path forwarded from the desktop is
    // guaranteed absent there (§Launch data, host-resolved values).
    //
    // Stored TRIMMED — the same rule as every identity component: the
    // launch gate validates the trimmed command, and a validator and writer
    // that disagree about the value would let "  buzz-agent  " pass the
    // gate and then make the harness spawn a filename containing spaces.
    if let Some(command) = launch.command.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        env.insert("BUZZ_ACP_AGENT_COMMAND".into(), command.to_string());
    }
    if !launch.args.is_empty() {
        // Comma-joined because that is what the harness's CLI parser decodes,
        // and what the desktop's local spawn does. An argument containing a
        // comma is unrepresentable in both paths; inventing an escaping
        // scheme here would produce args the harness cannot decode.
        env.insert("BUZZ_ACP_AGENT_ARGS".into(), launch.args.join(","));
    }
    env.insert("BUZZ_ACP_MCP_COMMAND".into(), "buzz-dev-mcp".into());

    if let Some(respond_to) = agent.respond_to.as_deref().filter(|s| !s.is_empty()) {
        validate_respond_to_gate(respond_to, agent.respond_to_allowlist.as_deref())?;
        env.insert("BUZZ_ACP_RESPOND_TO".into(), respond_to.to_string());
    }
    if let Some(list) = agent
        .respond_to_allowlist
        .as_ref()
        .filter(|l| !l.is_empty())
    {
        env.insert("BUZZ_ACP_RESPOND_TO_ALLOWLIST".into(), list.join(","));
    }

    if let Some(secs) = auth.inactivity_seconds {
        env.insert("BUZZ_ACP_EXIT_AFTER_INACTIVITY".into(), secs.to_string());
    }
    // The generation token doubles as the lifecycle-frame correlator, so
    // session logs and observer frames share one identity.
    env.insert(START_NONCE_KEY.into(), auth.generation.to_string());

    let total: usize = env.values().map(String::len).sum();
    if total > MAX_ENV_BYTES {
        return Err(format!(
            "agent environment is {total} bytes; this binding caps the \
             serialized environment at {MAX_ENV_BYTES}"
        ));
    }

    Ok(env)
}

/// Serialize a resolved environment to the `export` lines the launcher
/// sources (`set -a; . file; set +a` also works, but explicit `export` keeps
/// the file meaningful standalone).
///
/// Values are single-quoted with the POSIX `'\''` escape, which is a *total*
/// encoding: inside single quotes the shell interprets nothing, so every
/// byte sequence except the quote itself passes verbatim, and the quote is
/// spliced. Keys were already validated as POSIX names in [`build_env`] —
/// this serializer refuses to be reached with anything else.
pub fn serialize_exports(env: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    for (key, value) in env {
        debug_assert!(is_posix_env_key(key), "unvalidated key reached the serializer");
        out.push_str("export ");
        out.push_str(key);
        out.push_str("='");
        out.push_str(&value.replace('\'', r"'\''"));
        out.push_str("'\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload_json(extra_agent: serde_json::Value) -> AgentPayload {
        let mut agent = serde_json::json!({
            "name": "a",
            "relay_url": "wss://relay.example",
            "private_key_nsec": "nsec1example",
            "auth_tag": "tag-1",
        });
        let (serde_json::Value::Object(base), serde_json::Value::Object(extra)) =
            (&mut agent, extra_agent)
        else {
            panic!("expected objects")
        };
        base.extend(extra);
        serde_json::from_value(agent).unwrap()
    }

    fn build(agent: &AgentPayload) -> Result<BTreeMap<String, String>, String> {
        build_env(
            agent,
            AuthoritativeInputs {
                generation: "gen0001",
                inactivity_seconds: Some(7200),
                preapprove_agent_tools: true,
            },
        )
    }

    #[test]
    fn identity_comes_from_top_level_fields() {
        let env = build(&payload_json(serde_json::json!({}))).unwrap();
        assert_eq!(env["BUZZ_RELAY_URL"], "wss://relay.example");
        assert_eq!(env["BUZZ_PRIVATE_KEY"], "nsec1example");
        assert_eq!(env["NOSTR_PRIVATE_KEY"], "nsec1example");
        assert_eq!(env["BUZZ_AUTH_TAG"], "tag-1");
    }

    /// The spec's later-wins rule: a lower tier that spoofs an authoritative
    /// key is *overwritten*, not refused. Refusing would diverge from the
    /// local spawn, where the same env is written before the authoritative
    /// layer and simply loses.
    #[test]
    fn lower_tiers_cannot_spoof_authoritative_values() {
        let agent = payload_json(serde_json::json!({
            "launch": {
                "command": "goose",
                "policy_env": {
                    "BUZZ_PRIVATE_KEY": "nsec1attacker",
                    "BUZZ_MANAGED_AGENT_START_NONCE": "forged",
                },
                "env": {
                    "BUZZ_RELAY_URL": "wss://attacker.example",
                    "NOSTR_PRIVATE_KEY": "nsec1attacker",
                    "BUZZ_AUTH_TAG": "forged-tag",
                    "BUZZ_ACP_AGENT_OWNER": "cafe",
                    "BUZZ_ACP_AGENT_COMMAND": "/bin/sh",
                    "BUZZ_ACP_MCP_COMMAND": "/bin/sh",
                    "BUZZ_ACP_EXIT_AFTER_INACTIVITY": "0",
                },
                "owner_pubkey": "beef"
            }
        }));
        let env = build(&agent).unwrap();
        assert_eq!(env["BUZZ_PRIVATE_KEY"], "nsec1example");
        assert_eq!(env["NOSTR_PRIVATE_KEY"], "nsec1example");
        assert_eq!(env["BUZZ_RELAY_URL"], "wss://relay.example");
        assert_eq!(env["BUZZ_AUTH_TAG"], "tag-1");
        assert_eq!(env["BUZZ_ACP_AGENT_OWNER"], "beef");
        assert_eq!(env["BUZZ_ACP_AGENT_COMMAND"], "goose");
        assert_eq!(env["BUZZ_ACP_MCP_COMMAND"], "buzz-dev-mcp");
        assert_eq!(env["BUZZ_ACP_EXIT_AFTER_INACTIVITY"], "7200");
        assert_eq!(env["BUZZ_MANAGED_AGENT_START_NONCE"], "gen0001");
    }

    /// Tier 1 is *overridable* — user env beats policy defaults, matching the
    /// local spawn, where the user layer is written after them. Getting this
    /// backwards would make remote agents ignore overrides local agents honor.
    #[test]
    fn user_env_overrides_policy_defaults() {
        let agent = payload_json(serde_json::json!({
            "launch": {
                "policy_env": {"GOOSE_MODE": "auto", "BUZZ_ACP_MODEL": "sonnet"},
                "env": {"GOOSE_MODE": "chat"},
                "owner_pubkey": "beef"
            }
        }));
        let env = build(&agent).unwrap();
        assert_eq!(env["GOOSE_MODE"], "chat");
        assert_eq!(env["BUZZ_ACP_MODEL"], "sonnet");
    }

    /// `launch.env` already contains the merged user env, so re-merging the
    /// legacy field would undo a layering the desktop already resolved.
    #[test]
    fn legacy_env_vars_are_not_remerged_when_launch_present() {
        let agent = payload_json(serde_json::json!({
            "env_vars": {"STALE": "yes", "SHARED": "legacy"},
            "launch": {"env": {"SHARED": "resolved"}, "owner_pubkey": "beef"}
        }));
        let env = build(&agent).unwrap();
        assert_eq!(env["SHARED"], "resolved");
        assert!(!env.contains_key("STALE"), "legacy env_vars re-merged");
    }

    /// ...but a desktop predating the `launch` block has nothing else to
    /// offer, so the legacy field is the truth in exactly that case.
    #[test]
    fn legacy_env_vars_used_when_launch_absent() {
        let agent = payload_json(serde_json::json!({"env_vars": {"API": "v"}}));
        let env = build(&agent).unwrap();
        assert_eq!(env["API"], "v");
    }

    /// `preapprove_agent_tools` must reach the Codex runtime too: codex-acp
    /// never reads Claude Code's settings file — its tool policy is its
    /// mode, read from INITIAL_AGENT_MODE and defaulting to workspace-write
    /// "agent". Without this mapping, false still edits files and runs
    /// commands, and true still leaves escalated operations asking a
    /// harness that denies every request.
    #[test]
    fn codex_launches_carry_the_tool_policy_as_the_initial_mode() {
        let agent = payload_json(serde_json::json!({
            "launch": {"command": "codex-acp", "owner_pubkey": "beef"}
        }));
        let env = build(&agent).unwrap();
        assert_eq!(env["INITIAL_AGENT_MODE"], "agent-full-access");

        let env = build_env(
            &agent,
            AuthoritativeInputs {
                generation: "gen0001",
                inactivity_seconds: Some(7200),
                preapprove_agent_tools: false,
            },
        )
        .unwrap();
        assert_eq!(env["INITIAL_AGENT_MODE"], "read-only");
    }

    /// The mode is a tier-1 default: an explicit INITIAL_AGENT_MODE in the
    /// user's env wins, matching every other overridable behavior default.
    /// And it is scoped to the Codex command — other runtimes read their
    /// policy elsewhere, and a stray variable would be noise at best.
    #[test]
    fn the_codex_mode_is_overridable_and_scoped_to_codex() {
        let agent = payload_json(serde_json::json!({
            "launch": {
                "command": "codex-acp",
                "env": {"INITIAL_AGENT_MODE": "agent"},
                "owner_pubkey": "beef"
            }
        }));
        assert_eq!(build(&agent).unwrap()["INITIAL_AGENT_MODE"], "agent");

        for other in ["claude-agent-acp", "buzz-agent"] {
            let agent = payload_json(serde_json::json!({
                "launch": {"command": other, "owner_pubkey": "beef"}
            }));
            assert!(
                !build(&agent).unwrap().contains_key("INITIAL_AGENT_MODE"),
                "mode leaked to {other}"
            );
        }
    }

    #[test]
    fn refuses_when_no_owner_resolves() {
        let agent = payload_json(serde_json::json!({"auth_tag": null}));
        let err = build(&agent).unwrap_err();
        assert!(err.contains("!shutdown"), "unhelpful error: {err}");
    }

    /// An empty string is not an owner. Without this the refusal is
    /// bypassable by a blank field and the agent launches unable to be
    /// stopped.
    #[test]
    fn empty_owner_fields_count_as_absent() {
        for blank in ["", "   "] {
            let agent = payload_json(serde_json::json!({
                "auth_tag": blank,
                "launch": {"owner_pubkey": blank}
            }));
            assert!(
                build(&agent).is_err(),
                "whitespace resolved as an owner: {blank:?}"
            );
        }
    }

    /// The other half of every identity guard: what is *stored*. A validator
    /// that trims and a writer that doesn't disagree about the value, and the
    /// padding reaches the harness inside the env file. Assert on the stored
    /// string — asserting only that the deploy was accepted passes either way.
    #[test]
    fn identity_components_are_stored_trimmed() {
        let agent = payload_json(serde_json::json!({
            "relay_url": "  wss://relay.example  ",
            "auth_tag": "  tag-1  ",
            "launch": {"owner_pubkey": "  beefcafe  "}
        }));
        let env = build(&agent).unwrap();
        assert_eq!(env["BUZZ_RELAY_URL"], "wss://relay.example");
        assert_eq!(env["BUZZ_AUTH_TAG"], "tag-1");
        assert_eq!(env["BUZZ_ACP_AGENT_OWNER"], "beefcafe");
    }

    /// L1 item 1's third identity component. Without it an agent deploys with
    /// nothing to connect to — a session that starts, fails at the relay, and
    /// reads as a network fault rather than a refused launch.
    #[test]
    fn refuses_empty_relay_url() {
        for blank in ["", "   "] {
            let agent = payload_json(serde_json::json!({"relay_url": blank}));
            let err = build(&agent).unwrap_err();
            assert!(err.contains("relay_url"), "unhelpful error: {err}");
        }
    }

    #[test]
    fn owner_pubkey_alone_is_sufficient() {
        let agent = payload_json(serde_json::json!({
            "auth_tag": null,
            "launch": {"owner_pubkey": "beefcafe"}
        }));
        let env = build(&agent).unwrap();
        assert_eq!(env["BUZZ_ACP_AGENT_OWNER"], "beefcafe");
        assert!(!env.contains_key("BUZZ_AUTH_TAG"));
    }

    #[test]
    fn refuses_presence_suppression() {
        let agent = payload_json(serde_json::json!({
            "launch": {"env": {"BUZZ_ACP_NO_PRESENCE": "1"}, "owner_pubkey": "beef"}
        }));
        let err = build(&agent).unwrap_err();
        assert!(err.contains("BUZZ_ACP_NO_PRESENCE"), "got: {err}");
    }

    /// A non-POSIX key cannot be `export`ed by the launcher's shell — refuse
    /// with the key named rather than fail inside the sprite.
    #[test]
    fn refuses_non_posix_env_keys() {
        for bad in ["foo.bar", "foo-bar", "1LEADING", "", "has space"] {
            let agent = payload_json(serde_json::json!({
                "launch": {"env": {bad: "v"}, "owner_pubkey": "beef"}
            }));
            assert!(build(&agent).is_err(), "accepted non-POSIX key {bad:?}");
        }
    }

    /// The stored-value half of the launch gate: the gate validates the
    /// TRIMMED command, so the trimmed form must be what the harness
    /// receives — padding that survived to `BUZZ_ACP_AGENT_COMMAND` would
    /// pass validation and then spawn a filename containing spaces.
    #[test]
    fn agent_command_is_stored_trimmed() {
        let agent = payload_json(serde_json::json!({
            "launch": {"command": "  buzz-agent  ", "owner_pubkey": "beef"}
        }));
        let env = build(&agent).unwrap();
        assert_eq!(env["BUZZ_ACP_AGENT_COMMAND"], "buzz-agent");

        // Whitespace-only collapses to absent, matching the gate's reading.
        let agent = payload_json(serde_json::json!({
            "launch": {"command": "   ", "owner_pubkey": "beef"}
        }));
        assert!(!build(&agent).unwrap().contains_key("BUZZ_ACP_AGENT_COMMAND"));
    }

    #[test]
    fn args_are_comma_joined_and_omitted_when_empty() {
        let agent = payload_json(serde_json::json!({
            "launch": {"command": "goose", "args": ["run", "--no-session"], "owner_pubkey": "b"}
        }));
        let env = build(&agent).unwrap();
        assert_eq!(env["BUZZ_ACP_AGENT_ARGS"], "run,--no-session");

        let agent = payload_json(serde_json::json!({
            "launch": {"command": "goose", "args": [], "owner_pubkey": "b"}
        }));
        assert!(!build(&agent).unwrap().contains_key("BUZZ_ACP_AGENT_ARGS"));
    }

    /// The top-level `model`/`provider` fields are display inputs; their
    /// environment consequence is per-runtime and arrives already resolved
    /// inside `launch`. A provider-side mapping is wrong for three of the
    /// four built-in runtimes.
    #[test]
    fn provider_never_maps_model_or_provider_itself() {
        let agent = payload_json(serde_json::json!({
            "model": "claude-opus", "provider": "anthropic",
            "launch": {"owner_pubkey": "beef"}
        }));
        let env = build(&agent).unwrap();
        for key in [
            "BUZZ_AGENT_PROVIDER",
            "BUZZ_AGENT_MODEL",
            "GOOSE_PROVIDER",
            "GOOSE_MODEL",
        ] {
            assert!(!env.contains_key(key), "provider mapped {key} itself");
        }
    }

    /// `turn_timeout_seconds` is deprecated and ignored upstream; the local
    /// spawn does not emit it either.
    #[test]
    fn turn_timeout_is_not_mapped() {
        let agent = payload_json(serde_json::json!({
            "turn_timeout_seconds": 30, "launch": {"owner_pubkey": "b"}
        }));
        let env = build(&agent).unwrap();
        assert!(!env.keys().any(|k| k.contains("TURN_TIMEOUT")));
    }

    #[test]
    fn inactivity_omitted_when_unset() {
        let agent = payload_json(serde_json::json!({"launch": {"owner_pubkey": "b"}}));
        let env = build_env(
            &agent,
            AuthoritativeInputs {
                generation: "g",
                inactivity_seconds: None,
                preapprove_agent_tools: true,
            },
        )
        .unwrap();
        assert!(!env.contains_key("BUZZ_ACP_EXIT_AFTER_INACTIVITY"));
    }

    /// Structural guard over the whole authoritative list at once: whatever
    /// the lower tiers contain, no authoritative key holds a lower-tier value
    /// — including the conditionally-written ones the authoritative tier has
    /// nothing to say about, which must be **absent** rather than spoofed.
    /// (In the Kubernetes crate this test caught exactly that:
    /// `BUZZ_ACP_AGENT_ARGS` is only written when `launch.args` is non-empty,
    /// so plain later-wins overwrite left the spoofed value in place.)
    #[test]
    fn no_authoritative_key_retains_a_lower_tier_value() {
        let spoofed: serde_json::Map<String, serde_json::Value> = AUTHORITATIVE_KEYS
            .iter()
            .map(|k| ((*k).to_string(), serde_json::json!("SPOOFED")))
            .collect();
        // Split across both lower tiers: policy_env and env are separate
        // insertion points, and a fix that only cleared one would pass a
        // single-tier test.
        let agent = payload_json(serde_json::json!({
            "launch": {
                "policy_env": spoofed.clone(),
                "env": spoofed,
                "owner_pubkey": "beef",
            }
        }));
        let env = build(&agent).unwrap();
        for key in AUTHORITATIVE_KEYS {
            if let Some(value) = env.get(*key) {
                assert_ne!(value, "SPOOFED", "{key} kept a lower-tier value");
            }
        }
        // And the conditionally-written keys with nothing authoritative to
        // say are absent, not spoofed.
        for absent in ["BUZZ_ACP_AGENT_COMMAND", "BUZZ_ACP_AGENT_ARGS", "BUZZ_ACP_RESPOND_TO"] {
            assert!(!env.contains_key(absent), "{absent} survived the clear");
        }
    }

    fn pubkey(fill: char) -> String {
        std::iter::repeat_n(fill, 64).collect()
    }

    #[test]
    fn allowlist_mode_with_an_empty_list_is_refused() {
        for list in [serde_json::json!([]), serde_json::Value::Null] {
            let agent = payload_json(serde_json::json!({
                "respond_to": "allowlist", "respond_to_allowlist": list,
            }));
            assert!(build(&agent).is_err(), "empty allowlist accepted");
        }
    }

    #[test]
    fn an_allowlist_entry_that_is_not_64_hex_is_refused() {
        for bad in ["beefcafe", "npub1abc", ""] {
            let agent = payload_json(serde_json::json!({
                "respond_to": "allowlist", "respond_to_allowlist": [bad],
            }));
            assert!(build(&agent).is_err(), "accepted allowlist entry {bad:?}");
        }
    }

    #[test]
    fn a_valid_allowlist_gate_is_accepted_and_comma_joined() {
        let agent = payload_json(serde_json::json!({
            "respond_to": "allowlist",
            "respond_to_allowlist": [pubkey('a'), pubkey('b')],
        }));
        let env = build(&agent).unwrap();
        assert_eq!(env["BUZZ_ACP_RESPOND_TO"], "allowlist");
        assert_eq!(
            env["BUZZ_ACP_RESPOND_TO_ALLOWLIST"],
            format!("{},{}", pubkey('a'), pubkey('b'))
        );
    }

    /// The harness validates the allowlist only in allowlist mode; mirroring
    /// its asymmetry means a stale junk list never blocks an owner-only agent.
    #[test]
    fn a_junk_allowlist_is_tolerated_outside_allowlist_mode() {
        for mode in ["owner-only", "anyone", "nobody"] {
            let agent = payload_json(serde_json::json!({
                "respond_to": mode, "respond_to_allowlist": ["junk"],
            }));
            let env = build(&agent).unwrap();
            assert_eq!(env["BUZZ_ACP_RESPOND_TO"], mode);
        }
    }

    #[test]
    fn a_mode_the_harness_cannot_parse_is_refused() {
        for bad in ["npub1abc", "everyone", "ALLOWLIST"] {
            let agent = payload_json(serde_json::json!({"respond_to": bad}));
            assert!(build(&agent).is_err(), "accepted mode {bad:?}");
        }
    }

    #[test]
    fn a_padded_mode_is_refused_because_clap_does_not_trim() {
        for padded in [" allowlist ", "owner-only "] {
            let agent = payload_json(serde_json::json!({"respond_to": padded}));
            assert!(build(&agent).is_err(), "accepted padded mode {padded:?}");
        }
    }

    #[test]
    fn every_mode_the_harness_accepts_is_deployable() {
        for mode in RESPOND_TO_MODES {
            let allowlist = (mode == RESPOND_TO_ALLOWLIST).then(|| vec![pubkey('a')]);
            let agent = payload_json(serde_json::json!({
                "respond_to": mode, "respond_to_allowlist": allowlist,
            }));
            let env = build(&agent).unwrap();
            assert_eq!(env["BUZZ_ACP_RESPOND_TO"], mode);
        }
    }

    #[test]
    fn refuses_an_oversized_environment() {
        let agent = payload_json(serde_json::json!({
            "launch": {
                "env": {"BLOB": "x".repeat(MAX_ENV_BYTES + 1)},
                "owner_pubkey": "beef"
            }
        }));
        let err = build(&agent).unwrap_err();
        assert!(err.contains("bytes"), "got: {err}");
    }

    // ---- serialize_exports ----

    /// Single-quoting is a total encoding: whatever bytes a value carries,
    /// sourcing the serialized file must reproduce them exactly. Exercised
    /// with the values most likely to break a lesser scheme.
    #[test]
    fn exports_round_trip_hostile_values() {
        let hostile = [
            ("SIMPLE", "value"),
            ("QUOTE", "it's got 'quotes'"),
            ("DOUBLE", r#"say "hi" $(rm -rf /) `boom` \ $HOME"#),
            ("NEWLINE", "line1\nline2\n"),
            ("UNICODE", "héllo 世界 🚀"),
            ("EMPTY", ""),
            ("TRAILING", "spaces   "),
        ];
        let env: BTreeMap<String, String> = hostile
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let script = serialize_exports(&env);

        // Verify by actually sourcing it — the consumer is a shell, so the
        // oracle is a shell.
        let mut check = String::from("set -eu\n");
        check.push_str(&script);
        for (key, value) in &hostile {
            // Compare via printf %s to avoid echo's escape ambiguity.
            check.push_str(&format!(
                "[ \"$(printf %s \"${{{key}}}\")\" = \"$(printf %s {})\" ] || exit 9\n",
                shell_quote(value)
            ));
        }
        let status = std::process::Command::new("bash")
            .arg("-c")
            .arg(&check)
            .status()
            .expect("bash not available");
        assert!(status.success(), "round-trip failed:\n{script}");
    }

    fn shell_quote(value: &str) -> String {
        format!("'{}'", value.replace('\'', r"'\''"))
    }

    #[test]
    fn exports_are_deterministic_and_sorted() {
        let mut env = BTreeMap::new();
        env.insert("B".to_string(), "2".to_string());
        env.insert("A".to_string(), "1".to_string());
        let script = serialize_exports(&env);
        assert_eq!(script, "export A='1'\nexport B='2'\n");
    }
}
