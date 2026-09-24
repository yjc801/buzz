//! Git credential-helper env for a spawned agent, split out of `runtime.rs`
//! to keep that module under the desktop file-size ratchet.

/// Custom ACP harnesses do not run buzz-acp's Git bootstrap. Preserve the
/// Desktop-provided relay credential helper for those commands.
pub(super) fn apply_custom_acp_git_credentials(
    command: &mut std::process::Command,
    acp_command: &str,
    private_key: &str,
    relay_url: &str,
    credential_helper: Option<&std::path::Path>,
) {
    if acp_command == crate::managed_agents::DEFAULT_ACP_COMMAND {
        return;
    }
    let Some(helper) = credential_helper else {
        eprintln!(
            "buzz-desktop: git-credential-nostr not found — custom ACP command will not have automatic Buzz git auth"
        );
        return;
    };
    let relay_http_url = crate::relay::relay_http_base_url(relay_url);
    command.env("NOSTR_PRIVATE_KEY", private_key);
    command.env("GIT_TERMINAL_PROMPT", "0");
    command.env("GIT_CONFIG_COUNT", "2");
    command.env(
        "GIT_CONFIG_KEY_0",
        format!("credential.{relay_http_url}/git.helper"),
    );
    command.env(
        "GIT_CONFIG_VALUE_0",
        helper.to_string_lossy().replace('\\', "/"),
    );
    command.env(
        "GIT_CONFIG_KEY_1",
        format!("credential.{relay_http_url}/git.useHttpPath"),
    );
    command.env("GIT_CONFIG_VALUE_1", "true");
}
