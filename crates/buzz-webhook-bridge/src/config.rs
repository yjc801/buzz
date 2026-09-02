//! Environment configuration for the bridge daemon.
//!
//! Env-var configured, no clap — matching this repo's other daemons
//! (`buzz-waker`, `buzz-relay`, `buzz-pair-relay`). All parsing takes the
//! environment as an explicit `HashMap` (never reading the process
//! environment directly), matching `buzz-push-gateway`'s config convention,
//! so every failure mode is unit-testable without process-global env
//! mutation.
//!
//! | Env var | Required | Meaning |
//! |---|---|---|
//! | `BRIDGE_RELAY_URL` | yes | The relay to subscribe to (`wss://…`). |
//! | `BRIDGE_IDENTITY_NSEC` | yes | The bridge's own Nostr identity, hex or `nsec1…`. Needs relay membership (directly, or via the auth tag below). |
//! | `BRIDGE_AUTH_TAG` | no | NIP-OA authorization tag as a JSON array (`["auth", …]`), threaded into the NIP-42 AUTH event. Needed when the identity is not itself a relay member. |
//! | `BRIDGE_RULES` | one of | The rules document, inline JSON. |
//! | `BRIDGE_RULES_FILE` | one of | Path to the rules document. Exactly one of `BRIDGE_RULES` / `BRIDGE_RULES_FILE` must be set. |
//! | `RUST_LOG` | no | `tracing-subscriber` env filter; defaults to `buzz_webhook_bridge=info`. |

use std::collections::HashMap;

use nostr::{Keys, Tag};

use crate::rules::{parse_rules, Rule, RuleError};

/// Errors assembling a [`BridgeConfig`] from the environment.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A required variable is unset or empty.
    #[error("{0} is required but not set")]
    Missing(&'static str),
    /// `BRIDGE_IDENTITY_NSEC` does not parse as a Nostr key.
    #[error("invalid BRIDGE_IDENTITY_NSEC: {0}")]
    InvalidNsec(String),
    /// `BRIDGE_AUTH_TAG` is set but does not parse as a Nostr tag.
    #[error("invalid BRIDGE_AUTH_TAG: {0}")]
    InvalidAuthTag(String),
    /// Both `BRIDGE_RULES` and `BRIDGE_RULES_FILE` are set — ambiguous, so
    /// refused rather than picking one silently.
    #[error("set exactly one of BRIDGE_RULES and BRIDGE_RULES_FILE, not both")]
    BothRuleSources,
    /// Neither `BRIDGE_RULES` nor `BRIDGE_RULES_FILE` is set.
    #[error("set exactly one of BRIDGE_RULES and BRIDGE_RULES_FILE; neither is set")]
    NoRuleSource,
    /// `BRIDGE_RULES_FILE` could not be read.
    #[error("could not read BRIDGE_RULES_FILE {path}: {reason}")]
    UnreadableRulesFile {
        /// The configured path.
        path: String,
        /// The I/O error, as text.
        reason: String,
    },
    /// The rules document failed validation — see [`RuleError`].
    #[error(transparent)]
    Rules(#[from] RuleError),
}

/// The validated daemon configuration.
pub struct BridgeConfig {
    /// The relay to subscribe to.
    pub relay_url: String,
    /// The bridge's own identity — what NIP-42 authenticates as.
    pub keys: Keys,
    /// Optional NIP-OA authorization tag for the AUTH event.
    pub auth_tag: Option<Tag>,
    /// The validated rules, in document order.
    pub rules: Vec<Rule>,
}

fn required<'e>(
    env: &'e HashMap<String, String>,
    name: &'static str,
) -> Result<&'e str, ConfigError> {
    env.get(name)
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(ConfigError::Missing(name))
}

fn optional<'e>(env: &'e HashMap<String, String>, name: &str) -> Option<&'e str> {
    env.get(name)
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn parse_auth_tag(raw: Option<&str>) -> Result<Option<Tag>, ConfigError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let elements: Vec<String> = serde_json::from_str(raw).map_err(|error| {
        ConfigError::InvalidAuthTag(format!("not a JSON array of strings: {error}"))
    })?;
    let tag =
        Tag::parse(elements).map_err(|error| ConfigError::InvalidAuthTag(error.to_string()))?;
    Ok(Some(tag))
}

/// The rules document text, from whichever of the two sources is set.
///
/// `read_file` is injected so the both/neither/unreadable cases are testable
/// without touching a real filesystem; [`BridgeConfig::from_env`] passes
/// [`std::fs::read_to_string`].
fn rules_document(
    env: &HashMap<String, String>,
    read_file: impl Fn(&str) -> std::io::Result<String>,
) -> Result<String, ConfigError> {
    match (
        optional(env, "BRIDGE_RULES"),
        optional(env, "BRIDGE_RULES_FILE"),
    ) {
        (Some(_), Some(_)) => Err(ConfigError::BothRuleSources),
        (None, None) => Err(ConfigError::NoRuleSource),
        (Some(inline), None) => Ok(inline.to_string()),
        (None, Some(path)) => read_file(path).map_err(|error| ConfigError::UnreadableRulesFile {
            path: path.to_string(),
            reason: error.to_string(),
        }),
    }
}

impl BridgeConfig {
    /// Assemble and validate the full configuration from `env`.
    ///
    /// The same `env` feeds both the daemon's own variables and the rules'
    /// `${VAR}` expansion, so a secret referenced by a rule must be present
    /// in the same environment the daemon starts with.
    ///
    /// # Errors
    /// Any required variable is missing, the identity or auth tag does not
    /// parse, the rules source is ambiguous or unreadable, or any rule fails
    /// validation ([`RuleError`]).
    pub fn from_env(env: &HashMap<String, String>) -> Result<Self, ConfigError> {
        Self::from_env_with(env, |path| std::fs::read_to_string(path))
    }

    fn from_env_with(
        env: &HashMap<String, String>,
        read_file: impl Fn(&str) -> std::io::Result<String>,
    ) -> Result<Self, ConfigError> {
        let relay_url = required(env, "BRIDGE_RELAY_URL")?.to_string();
        let nsec = required(env, "BRIDGE_IDENTITY_NSEC")?;
        let keys =
            Keys::parse(nsec).map_err(|error| ConfigError::InvalidNsec(error.to_string()))?;
        let auth_tag = parse_auth_tag(optional(env, "BRIDGE_AUTH_TAG"))?;
        let document = rules_document(env, read_file)?;
        let rules = parse_rules(&document, env)?;
        Ok(Self {
            relay_url,
            keys,
            auth_tag,
            rules,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::nips::nip19::ToBech32;
    use serde_json::json;

    fn rules_json() -> String {
        json!([{
            "name": "example",
            "filter": { "kinds": [30023], "authors": ["ab".repeat(32)] },
            "webhook": {
                "url": "https://api.example.com/hook",
                "headers": { "Authorization": "Bearer ${HOOK_TOKEN}" }
            }
        }])
        .to_string()
    }

    fn base_env() -> HashMap<String, String> {
        let mut env = HashMap::new();
        env.insert(
            "BRIDGE_RELAY_URL".to_string(),
            "wss://relay.example.com".to_string(),
        );
        env.insert(
            "BRIDGE_IDENTITY_NSEC".to_string(),
            Keys::generate().secret_key().to_secret_hex(),
        );
        env.insert("BRIDGE_RULES".to_string(), rules_json());
        env.insert("HOOK_TOKEN".to_string(), "s3cret".to_string());
        env
    }

    #[test]
    fn a_complete_environment_parses() {
        let config = BridgeConfig::from_env(&base_env()).expect("valid env parses");
        assert_eq!(config.relay_url, "wss://relay.example.com");
        assert_eq!(config.rules.len(), 1);
        assert!(config.auth_tag.is_none());
    }

    #[test]
    fn each_required_variable_is_demanded_by_name() {
        for name in ["BRIDGE_RELAY_URL", "BRIDGE_IDENTITY_NSEC"] {
            let mut env = base_env();
            env.remove(name);
            match BridgeConfig::from_env(&env) {
                Err(ConfigError::Missing(missing)) => assert_eq!(missing, name),
                other => panic!(
                    "expected Missing({name}), got {other:?}",
                    other = other.err()
                ),
            }
        }
    }

    #[test]
    fn an_nsec_bech32_identity_also_parses() {
        let mut env = base_env();
        let keys = Keys::generate();
        env.insert(
            "BRIDGE_IDENTITY_NSEC".to_string(),
            keys.secret_key().to_bech32().expect("encodes"),
        );
        let config = BridgeConfig::from_env(&env).expect("nsec form parses");
        assert_eq!(config.keys.public_key(), keys.public_key());
    }

    #[test]
    fn a_garbage_identity_is_refused() {
        let mut env = base_env();
        env.insert("BRIDGE_IDENTITY_NSEC".to_string(), "not-a-key".to_string());
        assert!(matches!(
            BridgeConfig::from_env(&env),
            Err(ConfigError::InvalidNsec(_))
        ));
    }

    #[test]
    fn a_valid_auth_tag_is_threaded_through() {
        let mut env = base_env();
        env.insert(
            "BRIDGE_AUTH_TAG".to_string(),
            json!(["auth", "delegation-token"]).to_string(),
        );
        let config = BridgeConfig::from_env(&env).expect("parses");
        let tag = config.auth_tag.expect("a tag");
        assert_eq!(
            tag.as_slice(),
            &["auth".to_string(), "delegation-token".to_string()]
        );
    }

    #[test]
    fn a_malformed_auth_tag_is_refused_not_dropped() {
        let mut env = base_env();
        env.insert("BRIDGE_AUTH_TAG".to_string(), "not json".to_string());
        assert!(matches!(
            BridgeConfig::from_env(&env),
            Err(ConfigError::InvalidAuthTag(_))
        ));
    }

    #[test]
    fn both_rule_sources_at_once_are_refused() {
        let mut env = base_env();
        env.insert(
            "BRIDGE_RULES_FILE".to_string(),
            "/anywhere.json".to_string(),
        );
        assert!(matches!(
            BridgeConfig::from_env(&env),
            Err(ConfigError::BothRuleSources)
        ));
    }

    #[test]
    fn neither_rule_source_is_refused() {
        let mut env = base_env();
        env.remove("BRIDGE_RULES");
        assert!(matches!(
            BridgeConfig::from_env(&env),
            Err(ConfigError::NoRuleSource)
        ));
    }

    #[test]
    fn a_rules_file_is_read_and_an_unreadable_one_is_refused() {
        let mut env = base_env();
        env.remove("BRIDGE_RULES");
        env.insert(
            "BRIDGE_RULES_FILE".to_string(),
            "/etc/bridge/rules.json".to_string(),
        );

        let rules = rules_json();
        let config = BridgeConfig::from_env_with(&env, |path| {
            assert_eq!(path, "/etc/bridge/rules.json");
            Ok(rules.clone())
        })
        .expect("a readable file parses");
        assert_eq!(config.rules.len(), 1);

        let error = BridgeConfig::from_env_with(&env, |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no such file",
            ))
        })
        .err()
        .expect("an unreadable file is refused");
        assert!(matches!(error, ConfigError::UnreadableRulesFile { .. }));
    }

    #[test]
    fn a_rule_referencing_an_unset_variable_fails_config_assembly() {
        let mut env = base_env();
        env.remove("HOOK_TOKEN");
        assert!(matches!(
            BridgeConfig::from_env(&env),
            Err(ConfigError::Rules(RuleError::MissingEnvVar { .. }))
        ));
    }
}
