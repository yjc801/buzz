//! `buzz-webhook-bridge` daemon entry point.
//!
//! Env-var configured — this repo's daemons don't use clap; see
//! `crates/buzz-waker/src/main.rs` for the pattern this follows. See
//! [`buzz_webhook_bridge::config`] for the full variable table, and the
//! library docs for the loop-safety and delivery-semantics contracts.

use tokio_util::sync::CancellationToken;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

use buzz_webhook_bridge::bridge::run_bridge;
use buzz_webhook_bridge::config::BridgeConfig;

fn log_env_filter() -> EnvFilter {
    EnvFilter::new(
        std::env::var("RUST_LOG").unwrap_or_else(|_| "buzz_webhook_bridge=info".to_string()),
    )
}

/// Wait for SIGTERM (Unix) or Ctrl+C — matches
/// `crates/buzz-waker/src/main.rs`'s `shutdown_signal`.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(sigterm) => sigterm,
            Err(error) => {
                tracing::error!(
                    %error,
                    "buzz-webhook-bridge: could not install a SIGTERM handler; Ctrl+C only"
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
    // Install the ring CryptoProvider before the first TLS connection (the
    // wss:// relay socket and every https:// webhook call). Both ring and
    // aws-lc-rs are compiled in transitively, so rustls cannot auto-select
    // one and would panic at first use. A second install is a no-op, so the
    // result is ignored rather than unwrapped — same as buzz-waker.
    let _ = rustls::crypto::ring::default_provider().install_default();

    tracing_subscriber::registry()
        .with(fmt::layer().json().with_filter(log_env_filter()))
        .init();

    let env: std::collections::HashMap<String, String> = std::env::vars().collect();
    let config = BridgeConfig::from_env(&env)?;

    tracing::info!(
        relay_url = %config.relay_url,
        identity = %config.keys.public_key().to_hex(),
        rules = config.rules.len(),
        "buzz-webhook-bridge starting"
    );
    for rule in &config.rules {
        // Safe to log whole: url and header values Display as their
        // unexpanded templates (see `rules::Expanded`).
        tracing::info!(
            rule = %rule.name,
            kinds = ?rule.filter.kinds,
            authors = ?rule.filter.authors,
            d_prefix = ?rule.filter.d_prefix,
            url = %rule.webhook.url,
            max_per_minute = rule.max_per_minute,
            "rule loaded"
        );
    }

    let cancel = CancellationToken::new();
    let bridge_cancel = cancel.clone();
    let bridge = tokio::spawn(async move { run_bridge(&config, &bridge_cancel).await });

    shutdown_signal().await;
    tracing::info!("buzz-webhook-bridge: shutdown signal received; stopping");
    cancel.cancel();

    match bridge.await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(anyhow::anyhow!("HTTP client construction failed: {error}")),
        Err(join_error) => Err(anyhow::anyhow!("bridge task panicked: {join_error}")),
    }
}
