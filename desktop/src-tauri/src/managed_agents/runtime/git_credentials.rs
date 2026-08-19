//! Git credential-helper env for a spawned agent, split out of `runtime.rs`
//! to keep that module under the desktop file-size ratchet.

use std::process::Command;

use super::resolve_command;
use crate::managed_agents::ManagedAgentRecord;

/// Configure NIP-98 Buzz-relay git auth on a spawn `Command`.
pub(super) fn apply_git_credential_env(
    command: &mut Command,
    record: &ManagedAgentRecord,
    effective_relay_url: &str,
) {
    // Git credential helper: NIP-98 auth for Buzz relay git via git-credential-nostr.
    // Ephemeral GIT_CONFIG_COUNT env vars scoped to relay HTTP URL; NOSTR_PRIVATE_KEY mirrors BUZZ_PRIVATE_KEY.
    if let Some(cred_helper) = resolve_command("git-credential-nostr") {
        let relay_http_url = crate::relay::relay_http_base_url(effective_relay_url);

        command.env("NOSTR_PRIVATE_KEY", &record.private_key_nsec);
        command.env("GIT_TERMINAL_PROMPT", "0");
        command.env("GIT_CONFIG_COUNT", "2");
        command.env(
            "GIT_CONFIG_KEY_0",
            format!("credential.{relay_http_url}/git.helper"),
        );
        let helper = cred_helper.to_string_lossy().replace('\\', "/");
        command.env("GIT_CONFIG_VALUE_0", helper);
        command.env(
            "GIT_CONFIG_KEY_1",
            format!("credential.{relay_http_url}/git.useHttpPath"),
        );
        command.env("GIT_CONFIG_VALUE_1", "true");
    } else {
        eprintln!(
            "buzz-desktop: git-credential-nostr not found — agent {} will not have automatic Buzz git auth",
            record.name,
        );
    }
}
