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
/// a *rolling* release: Block re-publishes it and the baked digests below go
/// stale — deliberately, the same way the Kubernetes binding's baked image
/// digest moves with a provider upgrade (§Deploy State Machine divergence
/// note). A stale pin fails provision with an error naming the override.
pub const DEFAULT_SPRIG_VERSION: &str = "sprig-latest";

/// Digest pins for the sprig tarball, per target arch (verified against the
/// release's own `.sha256` manifests, 2026-08-06). The tarball is what runs
/// with the agent's private key, so a movable tag alone is not acceptable —
/// this is the binding's analog of the Kubernetes image digest pin.
pub const SPRIG_SHA256_X86_64: &str =
    "bdc2cea8ce4b754070ded93dacbbd27eadb0eb807daa1880156f241e3a6fcfa0";
pub const SPRIG_SHA256_AARCH64: &str =
    "a15544c918fd8e2aa64259e174b7511b422601cecd3e8a83896221d3ee001d0b";

/// Pinned npm versions of the ACP adapters provisioned when the corresponding
/// `install_*_adapter` flag is on. Baked provider state, like the sprig pins.
pub const CLAUDE_ADAPTER_VERSION: &str = "0.64.0";
pub const CODEX_ADAPTER_VERSION: &str = "1.1.7";

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
    /// Overrides the baked digest for the VM's arch. `None` = baked pin.
    pub sprig_sha256: Option<String>,
    pub install_claude_adapter: bool,
    pub install_codex_adapter: bool,
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

    let sprig_sha256 = optional_string(cfg, "sprig_sha256")?;
    if let Some(sha) = &sprig_sha256 {
        if sha.len() != 64 || !sha.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
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
        sprig_version: optional_string(cfg, "sprig_version")?
            .unwrap_or_else(|| DEFAULT_SPRIG_VERSION.to_string()),
        sprig_sha256,
        install_claude_adapter: optional_bool(cfg, "install_claude_adapter")?.unwrap_or(true),
        install_codex_adapter: optional_bool(cfg, "install_codex_adapter")?.unwrap_or(true),
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
                "description": "GitHub release tag of block/buzz to fetch the agent runtime from."
            },
            "sprig_sha256": {
                "type": "string",
                "title": "Sprig digest override",
                "description": "SHA-256 of the sprig tarball for this sprite's architecture. Leave empty for this provider's baked pin; overriding is your trust decision — the runtime holds the agent's private key."
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
        for zero in [json!({"inactivity_seconds": 0}), json!({"inactivity_seconds": "0"})] {
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
            parse(&json!({"inactivity_seconds": 900})).unwrap().inactivity_seconds,
            Some(900)
        );
        assert_eq!(
            parse(&json!({"inactivity_seconds": "900"})).unwrap().inactivity_seconds,
            Some(900)
        );
        assert_eq!(
            parse(&json!({"inactivity_seconds": ""})).unwrap().inactivity_seconds,
            Some(DEFAULT_INACTIVITY_SECONDS)
        );
        let err = parse(&json!({"inactivity_seconds": "soon"})).unwrap_err();
        assert!(err.contains("inactivity_seconds"), "{err}");
        let err = parse(&json!({"inactivity_seconds": -5})).unwrap_err();
        assert!(err.contains("non-negative"), "{err}");
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
        let cfg = parse(&json!({"install_claude_adapter": false, "install_codex_adapter": "false"}))
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
    fn schema_declares_exactly_the_six_v1_fields() {
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
