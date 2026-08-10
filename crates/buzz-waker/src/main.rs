//! `buzz-waker` daemon entry point.
//!
//! Env-var configured — this repo's other daemons don't use clap; see
//! `crates/buzz-relay/src/main.rs` and `crates/buzz-pair-relay/src/main.rs`
//! for the pattern this follows. JSON-structured logs, graceful shutdown on
//! SIGTERM/Ctrl+C via a shared [`CancellationToken`], and one mention-feed
//! loop ([`buzz_waker::wake_loop::run_wake_loop`]) plus one presence tap
//! ([`buzz_waker::presence_feed::run_presence_tap`]) spawned per configured
//! agent.
//!
//! # Configuration
//!
//! | Env var | Required | Meaning |
//! |---|---|---|
//! | `WAKER_RELAY_URL` | yes | The relay every watched agent's mention feed and presence tap connects to. |
//! | `WAKER_STATE_DIR` | yes | Base directory for durable per-agent state (`<dir>/<pubkey>/cursor.json`). Created if missing. |
//! | `WAKER_AGENTS_CONFIG_PATH` | yes | Path to a JSON file listing the agents to watch — see [`AgentConfig`]. |
//! | `RUST_LOG` | no | `tracing-subscriber` env filter. Defaults to `buzz_waker=info`. |
//!
//! # What is deliberately not here
//!
//! Bundle transport (how a signed launch bundle reaches this daemon) and the
//! provider deploy wire protocol are both out of scope for this build — see
//! `buzz_waker::effects`'s module doc. Agent identities and the watch list
//! are therefore read from local config rather than from a delivered bundle,
//! and every real wake attempt ends in `WakeOutcome::DeployFailed`, loudly
//! logged, rather than a faked success. Wiring a real signed-bundle source in
//! later only has to replace [`load_agents`] and the construction of
//! [`AgentConfig`] below; nothing in `wake_loop` or `effects` assumes local
//! config.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use nostr::{Keys, Tag};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use buzz_waker::decide::normalize_pubkey;
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

    let mut keys_by_agent: Vec<(Keys, Option<Tag>)> = Vec::with_capacity(agent_configs.len());
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
        keys_by_agent.push((keys, auth_tag));
    }

    // This daemon's whole known-agent baseline — see `effects`'s module doc
    // on why this is the accepted simplification for
    // `confirm_author_not_known_agent` rather than the full managed-agent
    // roster.
    let watch_list: Arc<[String]> = keys_by_agent
        .iter()
        .map(|(keys, _)| normalize_pubkey(&keys.public_key().to_hex()))
        .collect::<Vec<_>>()
        .into();

    std::fs::create_dir_all(&state_dir).map_err(|e| {
        anyhow::anyhow!(
            "could not create WAKER_STATE_DIR {}: {e}",
            state_dir.display()
        )
    })?;

    let cancel = CancellationToken::new();
    let mut tasks = Vec::with_capacity(keys_by_agent.len() * 2);

    for (keys, auth_tag) in keys_by_agent {
        let pubkey = normalize_pubkey(&keys.public_key().to_hex());
        let agent_dir = state_dir.join(&pubkey);
        std::fs::create_dir_all(&agent_dir).map_err(|e| {
            anyhow::anyhow!("could not create state dir {}: {e}", agent_dir.display())
        })?;
        let cursor_path = agent_dir.join("cursor.json");

        let presence_state = Arc::new(PresenceState::new());

        tracing::info!(agent = %pubkey, "buzz-waker: watching agent");

        {
            let relay_url = relay_url.clone();
            let keys = keys.clone();
            let auth_tag = auth_tag.clone();
            let presence_state = Arc::clone(&presence_state);
            let cancel = cancel.clone();
            tasks.push(tokio::spawn(async move {
                run_presence_tap(
                    &relay_url,
                    &keys,
                    auth_tag.as_ref(),
                    &presence_state,
                    &cancel,
                )
                .await;
            }));
        }

        {
            let config = WakeLoopConfig {
                relay_url: relay_url.clone(),
                keys,
                auth_tag,
                cursor_path,
                presence_state,
                watch_list: Arc::clone(&watch_list),
            };
            let cancel = cancel.clone();
            tasks.push(tokio::spawn(async move {
                run_wake_loop(config, cancel).await;
            }));
        }
    }

    shutdown_signal().await;
    tracing::info!("buzz-waker: shutdown signal received; stopping");
    cancel.cancel();

    for task in tasks {
        if let Err(error) = task.await {
            tracing::error!(%error, "buzz-waker: a watch task panicked during shutdown");
        }
    }

    tracing::info!("buzz-waker: shutdown complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_config_parses_the_documented_shape() {
        let json = r#"[{"nsec": "nsec1abc"}, {"nsec": "deadbeef", "auth_tag": ["auth", "token"]}]"#;
        let agents: Vec<AgentConfig> = serde_json::from_str(json).expect("parses");
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].auth_tag, None);
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
    fn a_valid_agent_list_round_trips_through_load_agents() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("agents.json");
        std::fs::write(&path, r#"[{"nsec": "deadbeef"}]"#).expect("write");

        let agents = load_agents(path.to_str().expect("utf8 path")).expect("loads");
        assert_eq!(agents.len(), 1);
    }
}
