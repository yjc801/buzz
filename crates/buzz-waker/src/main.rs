//! `buzz-waker` daemon entry point.
//!
//! Env-var configured — this repo's other daemons don't use clap; see
//! `crates/buzz-relay/src/main.rs` and `crates/buzz-pair-relay/src/main.rs`
//! for the pattern this follows. JSON-structured logs, graceful shutdown on
//! SIGTERM/Ctrl+C via a shared [`CancellationToken`], and three tasks spawned
//! per configured agent: the mention-feed loop
//! ([`buzz_waker::wake_loop::run_wake_loop`]), the presence tap
//! ([`buzz_waker::presence_feed::run_presence_tap`]), and the bundle-delivery
//! tap ([`buzz_waker::bundle_feed::run_bundle_tap`]).
//!
//! # Configuration
//!
//! | Env var | Required | Meaning |
//! |---|---|---|
//! | `WAKER_RELAY_URL` | yes | The relay every watched agent's mention feed, presence tap, and bundle tap connects to. |
//! | `WAKER_STATE_DIR` | yes | Base directory for durable per-agent state (`<dir>/<pubkey>/{cursor,floor}.json`). Created if missing. |
//! | `WAKER_AGENTS_CONFIG_PATH` | yes | Path to a JSON file listing the agents to watch — see [`AgentConfig`]. |
//! | `RUST_LOG` | no | `tracing-subscriber` env filter. Defaults to `buzz_waker=info`. |
//!
//! # What is still deliberately not here
//!
//! The "generation nonce" the bundle doc mentions as a second wake-specific
//! substitution has no concrete contract anywhere in this codebase yet — see
//! `buzz_waker::effects`'s module doc. Agent identities, the watch list, and
//! each agent's owner pubkey (pinned into its [`buzz_waker::floors::FloorStore`]
//! on first run, **G2**) are read from local config, matching the ecosystem's
//! existing agent-identity provisioning story — nothing about *that* pin can
//! come from a delivered bundle without defeating the pin's own purpose.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use nostr::{Keys, Tag};
use serde::Deserialize;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use buzz_waker::bundle_feed::{run_bundle_tap, BundleState};
use buzz_waker::decide::normalize_pubkey;
use buzz_waker::floors::FloorStore;
use buzz_waker::presence_feed::{run_presence_tap, PresenceState};
use buzz_waker::wake_loop::{run_wake_loop, WakeLoopConfig};

/// One watched agent, as read from `WAKER_AGENTS_CONFIG_PATH`.
#[derive(Debug, Deserialize)]
struct AgentConfig {
    /// The agent's own Nostr private key, hex or `nsec1...` — [`Keys::parse`]
    /// accepts either. This identity is what the mention feed and the
    /// presence tap both authenticate as, per §4 option (ii) of the design:
    /// one connection per watched agent, bound to that agent's own identity.
    nsec: String,
    /// Raw NIP-OA authorization tag, e.g. `["auth", "<token>"]`, if this
    /// relay deployment requires one to accept the connection. Most
    /// deployments do not — the interactive per-agent NIP-42 auth already
    /// identifies the connection correctly, and the deployment amendment in
    /// §4 of the design records that this fork's relay does not implement
    /// the restricted connection class this tag would otherwise request.
    #[serde(default)]
    auth_tag: Option<Vec<String>>,
    /// The owner's pubkey, hex — pinned into this agent's [`FloorStore`] on
    /// first run (**G2**) and used to scope the bundle-delivery query's
    /// `authors` filter. Read from local config, not from any delivered
    /// bundle: the pin has to exist independently of anything a bundle could
    /// claim about itself, or a compromised bundle could pin its own
    /// attacker-controlled owner.
    owner_pubkey: String,
}

/// Normalize and validate a configured `owner_pubkey` as a real Nostr public
/// key, before any per-agent state directory or [`FloorStore`] is created.
///
/// `owner_pubkey` is otherwise only normalized (trimmed/lowercased) and
/// never parsed until a bundle actually arrives and `bundle_feed` calls
/// [`nostr::PublicKey::from_hex`] on it — by which point a typo has already
/// been durably enrolled as the floor's pinned owner (G2, which never
/// rewrites once set). That leaves the daemon reporting healthy startup
/// while silently unable to admit anything, and correcting the config on a
/// later run is then refused by [`ensure_owner_pin_matches`]. Validating
/// here, before enrollment, catches the typo at startup instead.
///
/// # Errors
/// `owner_pubkey` does not parse as a hex Nostr public key.
fn parse_owner_pubkey(pubkey: &str, owner_pubkey: &str) -> anyhow::Result<String> {
    let owner_pubkey = normalize_pubkey(owner_pubkey);
    nostr::PublicKey::from_hex(&owner_pubkey).map_err(|e| {
        anyhow::anyhow!("invalid owner_pubkey for agent {pubkey} in WAKER_AGENTS_CONFIG_PATH: {e}")
    })?;
    Ok(owner_pubkey)
}

/// Refuse to start `pubkey`'s watch tasks if the floor's pinned owner
/// disagrees with what `WAKER_AGENTS_CONFIG_PATH` configures now.
///
/// The floor's owner is pinned once, at enrolment, and never rewritten (G2)
/// — but config is free to change on any later run. If it drifts from the
/// pin, the daemon would still start "successfully" while
/// subscribing/decrypting as the newly configured owner and verifying every
/// delivery against the old pinned one, so no bundle could ever be admitted.
/// Both arguments are expected already-normalized ([`normalize_pubkey`]).
///
/// # Errors
/// The two owners disagree.
fn ensure_owner_pin_matches(
    pubkey: &str,
    pinned_owner: &str,
    configured_owner: &str,
) -> anyhow::Result<()> {
    if pinned_owner != configured_owner {
        anyhow::bail!(
            "floor store for {pubkey} is pinned to owner {pinned_owner}, but \
             WAKER_AGENTS_CONFIG_PATH now configures owner {configured_owner}; refusing \
             to run with disagreeing owners"
        );
    }
    Ok(())
}

fn env_var(name: &str) -> anyhow::Result<String> {
    std::env::var(name).map_err(|_| anyhow::anyhow!("{name} is required but not set"))
}

fn log_env_filter() -> EnvFilter {
    EnvFilter::new(std::env::var("RUST_LOG").unwrap_or_else(|_| "buzz_waker=info".to_string()))
}

/// Load and validate the watch list from `path`.
///
/// # Errors
/// The file is missing or unreadable, its JSON does not match
/// [`AgentConfig`]'s shape, or it lists zero agents — a daemon with nothing
/// to watch is a misconfiguration, not a valid idle state.
fn load_agents(path: &str) -> anyhow::Result<Vec<AgentConfig>> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("could not read WAKER_AGENTS_CONFIG_PATH {path}: {e}"))?;
    let agents: Vec<AgentConfig> = serde_json::from_str(&raw)
        .map_err(|e| anyhow::anyhow!("could not parse WAKER_AGENTS_CONFIG_PATH {path}: {e}"))?;
    if agents.is_empty() {
        anyhow::bail!("WAKER_AGENTS_CONFIG_PATH {path} lists no agents; nothing to watch");
    }
    Ok(agents)
}

/// Wait for SIGTERM (Unix) or Ctrl+C — matches
/// `crates/buzz-relay/src/main.rs`'s `shutdown_signal`.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(sigterm) => sigterm,
            Err(error) => {
                tracing::error!(
                    %error,
                    "buzz-waker: could not install a SIGTERM handler; Ctrl+C only"
                );
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = sigterm.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer().json().with_filter(log_env_filter()))
        .init();

    let relay_url = env_var("WAKER_RELAY_URL")?;
    let state_dir = PathBuf::from(env_var("WAKER_STATE_DIR")?);
    let agents_config_path = env_var("WAKER_AGENTS_CONFIG_PATH")?;

    let agent_configs = load_agents(&agents_config_path)?;

    let mut keys_by_agent: Vec<(Keys, Option<Tag>, String)> =
        Vec::with_capacity(agent_configs.len());
    let mut seen_pubkeys = HashSet::new();
    for agent in agent_configs {
        let keys = Keys::parse(&agent.nsec)
            .map_err(|e| anyhow::anyhow!("invalid nsec in WAKER_AGENTS_CONFIG_PATH: {e}"))?;
        let pubkey = normalize_pubkey(&keys.public_key().to_hex());
        if !seen_pubkeys.insert(pubkey.clone()) {
            anyhow::bail!("WAKER_AGENTS_CONFIG_PATH lists {pubkey} more than once");
        }
        let auth_tag = agent
            .auth_tag
            .map(Tag::parse)
            .transpose()
            .map_err(|e| anyhow::anyhow!("invalid auth_tag for agent {pubkey}: {e}"))?;
        let owner_pubkey = parse_owner_pubkey(&pubkey, &agent.owner_pubkey)?;
        keys_by_agent.push((keys, auth_tag, owner_pubkey));
    }

    // This daemon's whole known-agent baseline — see `effects`'s module doc
    // on why this is the accepted simplification for
    // `confirm_author_not_known_agent` rather than the full managed-agent
    // roster.
    let watch_list: Arc<[String]> = keys_by_agent
        .iter()
        .map(|(keys, _, _)| normalize_pubkey(&keys.public_key().to_hex()))
        .collect::<Vec<_>>()
        .into();

    std::fs::create_dir_all(&state_dir).map_err(|e| {
        anyhow::anyhow!(
            "could not create WAKER_STATE_DIR {}: {e}",
            state_dir.display()
        )
    })?;

    let cancel = CancellationToken::new();
    let mut tasks: JoinSet<(String, &'static str)> = JoinSet::new();

    for (keys, auth_tag, owner_pubkey) in keys_by_agent {
        let pubkey = normalize_pubkey(&keys.public_key().to_hex());
        let agent_dir = state_dir.join(&pubkey);
        std::fs::create_dir_all(&agent_dir).map_err(|e| {
            anyhow::anyhow!("could not create state dir {}: {e}", agent_dir.display())
        })?;
        let cursor_path = agent_dir.join("cursor.json");
        let floor_path = agent_dir.join("floor.json");

        let presence_state = Arc::new(PresenceState::new());
        let bundle_state = Arc::new(BundleState::new());

        // Open-or-enroll, matching `CursorStore::open_or_start`'s idempotent
        // shape: a fresh state dir enrolls fresh, an existing one re-opens
        // its durable floors (G2) rather than resetting them.
        let mut floor_store = match FloorStore::open(&floor_path) {
            Ok(store) => store,
            Err(buzz_waker::floors::FloorError::NotEnrolled { .. }) => {
                FloorStore::enroll(&floor_path, &owner_pubkey).map_err(|e| {
                    anyhow::anyhow!("could not enroll floor store for {pubkey}: {e}")
                })?
            }
            Err(e) => {
                anyhow::bail!("could not open floor store for {pubkey}: {e}")
            }
        };

        let pinned_owner = normalize_pubkey(&floor_store.snapshot().owner_pubkey);
        ensure_owner_pin_matches(&pubkey, &pinned_owner, &owner_pubkey)?;

        tracing::info!(agent = %pubkey, owner = %owner_pubkey, "buzz-waker: watching agent");

        {
            let relay_url = relay_url.clone();
            let keys = keys.clone();
            let auth_tag = auth_tag.clone();
            let presence_state = Arc::clone(&presence_state);
            let cancel = cancel.clone();
            let pubkey = pubkey.clone();
            tasks.spawn(async move {
                run_presence_tap(
                    &relay_url,
                    &keys,
                    auth_tag.as_ref(),
                    &presence_state,
                    &cancel,
                )
                .await;
                (pubkey, "presence_tap")
            });
        }

        {
            let relay_url = relay_url.clone();
            let keys = keys.clone();
            let auth_tag = auth_tag.clone();
            let owner_pubkey = owner_pubkey.clone();
            let bundle_state = Arc::clone(&bundle_state);
            let cancel = cancel.clone();
            let pubkey = pubkey.clone();
            tasks.spawn(async move {
                run_bundle_tap(
                    &relay_url,
                    &keys,
                    auth_tag.as_ref(),
                    &owner_pubkey,
                    &mut floor_store,
                    &bundle_state,
                    &cancel,
                )
                .await;
                (pubkey, "bundle_tap")
            });
        }

        {
            let config = WakeLoopConfig {
                relay_url: relay_url.clone(),
                keys,
                auth_tag,
                cursor_path,
                presence_state,
                watch_list: Arc::clone(&watch_list),
                bundle_state,
            };
            let cancel = cancel.clone();
            let pubkey = pubkey.clone();
            tasks.spawn(async move {
                run_wake_loop(config, cancel).await;
                (pubkey, "wake_loop")
            });
        }
    }

    // A watch task can also finish on its own, outside the shutdown path: a
    // corrupt cursor makes `run_wake_loop` return immediately, and either
    // task can panic. If that happens before `cancel` fires, the daemon must
    // not keep running with that agent silently unwatched and reporting
    // healthy — race the first such completion against the shutdown signal
    // and treat an early one as fatal for the whole process.
    let early_exit = tokio::select! {
        () = shutdown_signal() => {
            tracing::info!("buzz-waker: shutdown signal received; stopping");
            None
        }
        result = tasks.join_next() => Some(result),
    };

    cancel.cancel();

    while let Some(result) = tasks.join_next().await {
        if let Err(error) = result {
            tracing::error!(%error, "buzz-waker: a watch task panicked during shutdown");
        }
    }

    tracing::info!("buzz-waker: shutdown complete");

    match early_exit {
        Some(Some(Ok((pubkey, task)))) => {
            anyhow::bail!(
                "buzz-waker: {task} for agent {pubkey} exited before shutdown was requested; \
                 that agent stopped being watched — treating as fatal rather than running \
                 silently degraded"
            )
        }
        Some(Some(Err(error))) => {
            anyhow::bail!(
                "buzz-waker: a watch task panicked before shutdown was requested: {error}"
            )
        }
        Some(None) | None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_config_parses_the_documented_shape() {
        let owner = "a".repeat(64);
        let json = format!(
            r#"[{{"nsec": "nsec1abc", "owner_pubkey": "{owner}"}}, {{"nsec": "deadbeef", "auth_tag": ["auth", "token"], "owner_pubkey": "{owner}"}}]"#
        );
        let json = json.as_str();
        let agents: Vec<AgentConfig> = serde_json::from_str(json).expect("parses");
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].auth_tag, None);
        assert_eq!(agents[0].owner_pubkey, owner);
        assert_eq!(
            agents[1].auth_tag,
            Some(vec!["auth".to_string(), "token".to_string()])
        );
    }

    #[test]
    fn loading_an_empty_agent_list_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agents.json");
        std::fs::write(&path, "[]").expect("write");

        let result = load_agents(path.to_str().expect("utf8 path"));
        assert!(result.is_err());
    }

    #[test]
    fn loading_a_missing_file_reports_a_clear_error() {
        let result = load_agents("/nonexistent/path/agents.json");
        assert!(result.is_err());
    }

    #[test]
    fn a_valid_owner_pubkey_is_normalized_and_accepted() {
        let owner = "A".repeat(64);
        let parsed = parse_owner_pubkey("agent", &owner).expect("valid hex pubkey parses");
        assert_eq!(parsed, "a".repeat(64));
    }

    #[test]
    fn a_malformed_owner_pubkey_is_refused_before_enrollment() {
        let error = parse_owner_pubkey("agent", "not-a-key").unwrap_err();
        assert!(
            error.to_string().contains("invalid owner_pubkey"),
            "{error}"
        );
    }

    #[test]
    fn a_pin_matching_the_configured_owner_is_accepted() {
        let owner = normalize_pubkey(&"a".repeat(64));
        assert!(ensure_owner_pin_matches("agent", &owner, &owner).is_ok());
    }

    #[test]
    fn a_pin_disagreeing_with_the_configured_owner_is_refused() {
        let pinned = normalize_pubkey(&"a".repeat(64));
        let configured = normalize_pubkey(&"b".repeat(64));
        let error = ensure_owner_pin_matches("agent", &pinned, &configured).unwrap_err();
        assert!(error.to_string().contains("disagreeing owners"), "{error}");
    }

    #[test]
    fn a_valid_agent_list_round_trips_through_load_agents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agents.json");
        std::fs::write(
            &path,
            format!(
                r#"[{{"nsec": "deadbeef", "owner_pubkey": "{}"}}]"#,
                "a".repeat(64)
            ),
        )
        .expect("write");

        let agents = load_agents(path.to_str().expect("utf8 path")).expect("loads");
        assert_eq!(agents.len(), 1);
    }
}
