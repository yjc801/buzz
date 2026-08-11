//! Desktop-side wiring for the provider deploy wire protocol.
//!
//! The protocol itself — staging, negotiation, invocation, config
//! validation, PATH discovery — lives in `buzz-provider-deploy`, shared with
//! `buzz-waker` so both talk to a `buzz-backend-<id>` binary the same way.
//! This module re-exports that crate's API and adds the one thing that is
//! genuinely desktop-specific: resolving `~/.buzz` as the deployed process's
//! working directory.

pub use buzz_provider_deploy_pkg::*;

/// Invoke a provider binary using the desktop's own agent working directory.
///
/// Thin wrapper over [`buzz_provider_deploy_pkg::invoke_provider`] — see its
/// doc for the protocol itself.
pub fn invoke_provider(
    binary: &std::path::Path,
    request: &serde_json::Value,
    timeout: std::time::Duration,
) -> Result<serde_json::Value, String> {
    buzz_provider_deploy_pkg::invoke_provider(
        binary,
        request,
        timeout,
        super::default_agent_workdir().as_deref(),
    )
}

/// Deploy through the desktop's own agent working directory, with no digest
/// pin: the desktop resolves the provider binary itself and never receives
/// a signed launch bundle, so it has no pinned digest to check against.
///
/// Thin wrapper over [`buzz_provider_deploy_pkg::provider_deploy`] — see its doc.
pub fn provider_deploy(
    binary: &std::path::Path,
    agent: &serde_json::Value,
    provider_config: &serde_json::Value,
) -> Result<buzz_provider_deploy_pkg::ProviderDeployOutcome, String> {
    buzz_provider_deploy_pkg::provider_deploy(
        binary,
        agent,
        provider_config,
        super::default_agent_workdir().as_deref(),
    )
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendProviderInfo {
    pub id: String,
    pub binary_path: String,
}
