//! Ambient Sprites API credential resolution (spec I2: credentials never
//! transit `provider_config`).
//!
//! Resolution order: `SPRITE_TOKEN` → `SPRITES_TOKEN` → the macOS keychain
//! entry the sprite CLI created (`sprite login`), located via the CLI's own
//! metadata under `~/.sprites`. The env arms serve CLI/CI/tests; the keychain
//! arm is the production path for a Finder-launched desktop, whose providers
//! inherit launchd's minimal environment.
//!
//! The CLI's newer storage layout keeps the token either in the keychain
//! (`keyring_key` names the service) or encrypted on disk (`*.token.enc`).
//! The encrypted file's scheme is undocumented and this provider never
//! attempts to decrypt it — that install resolves through the keychain or
//! not at all.
//!
//! Every error names the sources checked and the remedies; no error ever
//! contains a token value.

use std::path::{Path, PathBuf};

/// A resolved credential. `source` is safe to surface in diagnostics; the
/// token itself must never appear in any output (§Provider Output Is
/// Untrusted — the desktop scrubs `sprt_tok_…`, and we do not rely on it).
pub struct Credential {
    pub token: String,
    pub source: &'static str,
}

/// Deliberately manual, deliberately redacting: a derived `Debug` would make
/// the token one accidental `{:?}` away from a log line.
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credential")
            .field("token", &"<redacted>")
            .field("source", &self.source)
            .finish()
    }
}

const FAIL_REMEDY: &str = "Run `sprite login` (stores the token in the macOS keychain), or set \
     SPRITE_TOKEN in the environment Buzz Desktop is launched from.";

/// Resolve a credential from the real process environment, home directory,
/// and keychain.
pub fn resolve(org_override: Option<&str>) -> Result<Credential, String> {
    let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
        format!("could not resolve a Sprites API credential: HOME is unset. {FAIL_REMEDY}")
    })?;
    resolve_with(
        |key| std::env::var(key).ok(),
        &home,
        org_override,
        keychain_lookup,
    )
}

/// The pure resolution chain, with the environment, home, and keychain reads
/// injected so every arm is unit-testable without a real keychain.
fn resolve_with(
    env: impl Fn(&str) -> Option<String>,
    home: &Path,
    org_override: Option<&str>,
    lookup: impl Fn(&KeyringEntry) -> Option<String>,
) -> Result<Credential, String> {
    for (var, source) in [
        ("SPRITE_TOKEN", "env:SPRITE_TOKEN"),
        ("SPRITES_TOKEN", "env:SPRITES_TOKEN"),
    ] {
        if let Some(value) = env(var) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Ok(Credential {
                    token: trimmed.to_string(),
                    source,
                });
            }
        }
    }

    if let Some(entry) = keyring_entry(home, org_override) {
        if let Some(value) = lookup(&entry) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Ok(Credential {
                    token: trimmed.to_string(),
                    source: "keychain",
                });
            }
        }
    }

    Err(format!(
        "could not resolve a Sprites API credential: SPRITE_TOKEN and SPRITES_TOKEN \
         are unset and no keychain-backed sprite CLI login was found under \
         ~/.sprites. {FAIL_REMEDY}"
    ))
}

/// A keychain generic-password coordinate: the CLI stores tokens under
/// service `sprites-cli:<user-id>` with the org's `keyring_key` value as the
/// **account** (verified against a live keychain, 2026-08-06 — the
/// `keyring_key` alone is not a service name).
pub struct KeyringEntry {
    pub service: String,
    pub account: String,
}

/// Walk the sprite CLI's metadata to the keychain coordinate for the
/// selected org: `~/.sprites/sprites.json` → `current_user` + selection →
/// the user file named by `users[].config_path` →
/// `urls.<url>.orgs.<org>.keyring_key`.
///
/// Any missing or malformed step yields `None` — the chain falls through to
/// the final error rather than failing on a layout this provider does not
/// own (the CLI documents the format as subject to change).
fn keyring_entry(home: &Path, org_override: Option<&str>) -> Option<KeyringEntry> {
    let root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.join(".sprites/sprites.json")).ok()?)
            .ok()?;

    let url = root
        .get("current_selection")?
        .get("url")?
        .as_str()
        .unwrap_or("https://api.sprites.dev");
    let org = match org_override {
        Some(org) => org.to_string(),
        None => root
            .get("current_selection")?
            .get("org")?
            .as_str()?
            .to_string(),
    };

    // Newer split layout: a per-user config file. Its path comes from the
    // root metadata's own `users[].config_path` pointer (the on-disk name
    // carries a suffix the id alone does not predict — verified live);
    // joining `users/<id>.json` is only the fallback for metadata that
    // predates the pointer.
    let current_user = root.get("current_user").and_then(|u| u.as_str());
    let user_file = if let Some(user_id) = current_user {
        let pointed = root
            .get("users")
            .and_then(|users| users.as_array())
            .and_then(|users| {
                users
                    .iter()
                    .find(|u| u.get("id").and_then(|id| id.as_str()) == Some(user_id))
            })
            .and_then(|u| u.get("config_path"))
            .and_then(|p| p.as_str())
            .map(PathBuf::from);
        let path =
            pointed.unwrap_or_else(|| home.join(".sprites/users").join(format!("{user_id}.json")));
        let user: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
        Some(user)
    } else {
        None
    };
    // Older single-file layout keeps `urls` at the root.
    let tree = user_file.as_ref().unwrap_or(&root);

    let account = tree
        .get("urls")?
        .get(url)?
        .get("orgs")?
        .get(&org)?
        .get("keyring_key")?
        .as_str()?
        .to_string();
    // With a current_user the service is `sprites-cli:<user-id>`; the legacy
    // single-file layout used the keyring_key as the service itself.
    let service = match current_user {
        Some(user_id) => format!("sprites-cli:{user_id}"),
        None => account.clone(),
    };
    Some(KeyringEntry { service, account })
}

/// Read a generic password from the macOS keychain, via the absolute
/// `security` path (this process may run with launchd's minimal PATH).
/// First use triggers the OS's one-time Allow prompt attributed to this
/// binary; the deploy budget (600s) absorbs the wait. A denial or a missing
/// entry both read as `None`.
fn keychain_lookup(entry: &KeyringEntry) -> Option<String> {
    let out = std::process::Command::new("/usr/bin/security")
        .args([
            "find-generic-password",
            "-w",
            "-s",
            &entry.service,
            "-a",
            &entry.account,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let token = String::from_utf8(out.stdout).ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_env(_: &str) -> Option<String> {
        None
    }
    fn no_keychain(_: &KeyringEntry) -> Option<String> {
        None
    }

    /// Build a `~/.sprites` tree in the newer split layout, with the user
    /// file at a suffixed path only the `config_path` pointer names —
    /// mirroring the live CLI (verified 2026-08-06), so a walk that guesses
    /// `users/<id>.json` fails this fixture.
    fn sprites_home(dir: &Path, org: &str, keyring_key: &str) {
        std::fs::create_dir_all(dir.join(".sprites/users")).unwrap();
        let config_path = dir.join(".sprites/users/user-1-abcd1234.json");
        std::fs::write(
            dir.join(".sprites/sprites.json"),
            serde_json::json!({
                "version": "1",
                "current_selection": {"url": "https://api.sprites.dev", "org": org},
                "current_user": "user-1",
                "users": [{"id": "user-1", "config_path": config_path}],
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            config_path,
            serde_json::json!({
                "urls": {"https://api.sprites.dev": {"orgs": {org: {
                    "name": org, "keyring_key": keyring_key,
                }}}}
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn env_vars_win_in_order() {
        let tmp = std::env::temp_dir();
        let cred = resolve_with(
            |k| match k {
                "SPRITE_TOKEN" => Some("  tok-a  ".into()),
                "SPRITES_TOKEN" => Some("tok-b".into()),
                _ => None,
            },
            &tmp,
            None,
            no_keychain,
        )
        .unwrap();
        assert_eq!(cred.token, "tok-a");
        assert_eq!(cred.source, "env:SPRITE_TOKEN");

        let cred = resolve_with(
            |k| (k == "SPRITES_TOKEN").then(|| "tok-b".into()),
            &tmp,
            None,
            no_keychain,
        )
        .unwrap();
        assert_eq!(cred.token, "tok-b");
    }

    #[test]
    fn blank_env_values_fall_through() {
        let tmp = std::env::temp_dir();
        let err = resolve_with(|_| Some("   ".into()), &tmp, None, no_keychain).unwrap_err();
        assert!(err.contains("Sprites API credential"), "{err}");
    }

    #[test]
    fn keychain_arm_uses_the_cli_metadata() {
        let dir = std::env::temp_dir().join(format!("sprites-cred-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        sprites_home(&dir, "my-org", "sprites:org:https://api.sprites.dev:my-org");

        // Service is `sprites-cli:<user-id>`; the keyring_key value is the
        // ACCOUNT (the live keychain's real coordinate scheme).
        let cred = resolve_with(no_env, &dir, None, |entry| {
            (entry.service == "sprites-cli:user-1"
                && entry.account == "sprites:org:https://api.sprites.dev:my-org")
                .then(|| "tok-k\n".into())
        })
        .unwrap();
        assert_eq!(cred.token, "tok-k");
        assert_eq!(cred.source, "keychain");

        // An org override that has no keyring entry falls through to the error.
        let err = resolve_with(no_env, &dir, Some("other-org"), |_| {
            panic!("no entry should be derivable for an unknown org")
        })
        .unwrap_err();
        assert!(err.contains("sprite login"), "{err}");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The failure message names every source and remedy — and never any
    /// token-shaped value.
    #[test]
    fn failure_names_sources_and_remedies() {
        let dir = std::env::temp_dir().join(format!("sprites-cred-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let err = resolve_with(no_env, &dir, None, no_keychain).unwrap_err();
        for needle in [
            "SPRITE_TOKEN",
            "SPRITES_TOKEN",
            "~/.sprites",
            "sprite login",
        ] {
            assert!(err.contains(needle), "missing {needle}: {err}");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
