use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde::Serialize;

const WAGGLE_RELEASE_IDENTIFIER_PREFIX: &str = "xyz.waggle.app";
const BUZZ_RELEASE_IDENTIFIER_PREFIX: &str = "xyz.block.buzz.app";
const SPROUT_RELEASE_IDENTIFIER: &str = "xyz.block.sprout.app";
const WAGGLE_DEV_IDENTIFIER_PREFIX: &str = "xyz.waggle.app.dev";
const BUZZ_DEV_IDENTIFIER_PREFIX: &str = "xyz.block.buzz.app.dev";
const SPROUT_DEV_IDENTIFIER_PREFIX: &str = "xyz.block.sprout.app.dev";

/// localStorage key names as written by each generation's frontend. Buzz
/// renamed Sprout's `sprout-workspaces`/`sprout-active-workspace-id` to
/// `buzz-communities`/`buzz-active-community-id`; Waggle kept Buzz's names
/// (its own store is seeded via [`get_legacy_workspace_storage`], not a
/// rename), so only two key sets ever need reading, not three.
struct LegacyKeys {
    workspaces: &'static str,
    active_workspace: &'static str,
    onboarding_complete_prefix: &'static str,
}

const SPROUT_KEYS: LegacyKeys = LegacyKeys {
    workspaces: "sprout-workspaces",
    active_workspace: "sprout-active-workspace-id",
    onboarding_complete_prefix: "sprout-onboarding-complete.v1:",
};

const BUZZ_KEYS: LegacyKeys = LegacyKeys {
    workspaces: "buzz-communities",
    active_workspace: "buzz-active-community-id",
    onboarding_complete_prefix: "buzz-onboarding-complete.v1:",
};

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyWorkspaceStorage {
    workspaces: Option<String>,
    active_workspace_id: Option<String>,
    onboarding_completions: Vec<LegacyOnboardingCompletion>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyOnboardingCompletion {
    pubkey: String,
    value: String,
}

/// Ancestor identifiers to check for legacy state, nearest generation first:
/// Waggle checks Buzz then Sprout; a still-current Buzz build (should one ever
/// run this code) checks only Sprout. Dev prefixes are matched before release
/// prefixes since `xyz.waggle.app.dev` also starts with `xyz.waggle.app`.
fn legacy_ancestors(current_identifier: &str) -> Vec<(String, LegacyKeys)> {
    if let Some(rest) = current_identifier.strip_prefix(WAGGLE_DEV_IDENTIFIER_PREFIX) {
        vec![
            (format!("{BUZZ_DEV_IDENTIFIER_PREFIX}{rest}"), BUZZ_KEYS),
            (format!("{SPROUT_DEV_IDENTIFIER_PREFIX}{rest}"), SPROUT_KEYS),
        ]
    } else if let Some(rest) = current_identifier.strip_prefix(WAGGLE_RELEASE_IDENTIFIER_PREFIX) {
        vec![
            (format!("{BUZZ_RELEASE_IDENTIFIER_PREFIX}{rest}"), BUZZ_KEYS),
            (format!("{SPROUT_RELEASE_IDENTIFIER}{rest}"), SPROUT_KEYS),
        ]
    } else if let Some(rest) = current_identifier.strip_prefix(BUZZ_DEV_IDENTIFIER_PREFIX) {
        vec![(format!("{SPROUT_DEV_IDENTIFIER_PREFIX}{rest}"), SPROUT_KEYS)]
    } else if let Some(rest) = current_identifier.strip_prefix(BUZZ_RELEASE_IDENTIFIER_PREFIX) {
        vec![(format!("{SPROUT_RELEASE_IDENTIFIER}{rest}"), SPROUT_KEYS)]
    } else {
        Vec::new()
    }
}

#[cfg(target_os = "macos")]
fn legacy_webkit_data_root(identifier: &str) -> Option<PathBuf> {
    dirs::home_dir().map(|home| {
        home.join("Library")
            .join("WebKit")
            .join(identifier)
            .join("WebsiteData")
    })
}

#[cfg(not(target_os = "macos"))]
fn legacy_webkit_data_root(_identifier: &str) -> Option<PathBuf> {
    None
}

fn collect_local_storage_databases(root: &Path, databases: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_local_storage_databases(&path, databases);
        } else if path.file_name().and_then(|name| name.to_str()) == Some("localstorage.sqlite3") {
            databases.push(path);
        }
    }
}

fn decode_webkit_local_storage_value(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return Some(String::new());
    }

    if bytes.len().is_multiple_of(2) {
        let has_utf16_ascii_shape = bytes.chunks_exact(2).any(|chunk| chunk[1] == 0);
        if has_utf16_ascii_shape {
            let utf16: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                .collect();
            if let Ok(value) = String::from_utf16(&utf16) {
                return Some(value.trim_end_matches('\0').to_string());
            }
        }
    }

    String::from_utf8(bytes.to_vec()).ok()
}

fn read_legacy_workspace_storage_db(
    path: &Path,
    keys: &LegacyKeys,
) -> Result<LegacyWorkspaceStorage, String> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| format!("open legacy localStorage db: {e}"))?;

    let mut stmt = conn
        .prepare(
            "SELECT key, value FROM ItemTable \
             WHERE key = ?1 OR key = ?2 OR key LIKE ?3",
        )
        .map_err(|e| format!("prepare legacy localStorage query: {e}"))?;
    let mut rows = stmt
        .query([
            keys.workspaces,
            keys.active_workspace,
            &format!("{}%", keys.onboarding_complete_prefix),
        ])
        .map_err(|e| format!("query legacy localStorage: {e}"))?;

    let mut result = LegacyWorkspaceStorage::default();
    while let Some(row) = rows
        .next()
        .map_err(|e| format!("read legacy localStorage row: {e}"))?
    {
        let key: String = row
            .get(0)
            .map_err(|e| format!("read legacy localStorage key: {e}"))?;
        let value_bytes: Vec<u8> = row
            .get(1)
            .map_err(|e| format!("read legacy localStorage value: {e}"))?;
        let Some(value) = decode_webkit_local_storage_value(&value_bytes) else {
            continue;
        };

        if key == keys.workspaces {
            result.workspaces = Some(value);
        } else if key == keys.active_workspace {
            result.active_workspace_id = Some(value);
        } else if let Some(pubkey) = key.strip_prefix(keys.onboarding_complete_prefix) {
            result
                .onboarding_completions
                .push(LegacyOnboardingCompletion {
                    pubkey: pubkey.to_string(),
                    value,
                });
        }
    }

    Ok(result)
}

fn merge_legacy_workspace_storage(
    target: &mut LegacyWorkspaceStorage,
    source: LegacyWorkspaceStorage,
) {
    if target.workspaces.is_none() {
        target.workspaces = source.workspaces;
    }
    if target.active_workspace_id.is_none() {
        target.active_workspace_id = source.active_workspace_id;
    }
    target
        .onboarding_completions
        .extend(source.onboarding_completions);
}

/// Return workspace-scoped localStorage values from the prior generations'
/// WebKit data directories (Buzz, then Sprout) so the frontend can seed
/// Waggle localStorage before first render. This is separate from
/// `migrate_legacy_app_data_dir`: Tauri app data migration copies files such
/// as `identity.key`, but WebKit localStorage lives under
/// `~/Library/WebKit/<identifier>/...` on macOS and is not included in the
/// app data directory.
///
/// Checks ancestors nearest-first (Buzz before Sprout): `merge_legacy_workspace_storage`
/// only fills fields the nearer ancestor left `None`, so a user who has real
/// Buzz community state never has it clobbered by an older Sprout snapshot,
/// while a user who skipped straight from Sprout still gets seeded.
#[tauri::command]
pub async fn get_legacy_workspace_storage(
    app: tauri::AppHandle,
) -> Result<LegacyWorkspaceStorage, String> {
    let identifier = app.config().identifier.clone();
    tokio::task::spawn_blocking(move || {
        let mut result = LegacyWorkspaceStorage::default();
        for (ancestor_identifier, keys) in legacy_ancestors(&identifier) {
            let Some(root) = legacy_webkit_data_root(&ancestor_identifier) else {
                continue;
            };
            if !root.exists() {
                continue;
            }

            let mut databases = Vec::new();
            collect_local_storage_databases(&root, &mut databases);

            for database in databases {
                match read_legacy_workspace_storage_db(&database, &keys) {
                    Ok(storage) => merge_legacy_workspace_storage(&mut result, storage),
                    Err(error) => eprintln!(
                        "buzz-desktop: legacy-local-storage-migration: {}: {error}",
                        database.display()
                    ),
                }
            }
        }

        Ok(result)
    })
    .await
    .map_err(|e| format!("spawn_blocking failed: {e}"))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_ancestors_of_waggle_release_are_buzz_then_sprout() {
        let ancestors = legacy_ancestors("xyz.waggle.app");
        let identifiers: Vec<&str> = ancestors.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(identifiers, ["xyz.block.buzz.app", "xyz.block.sprout.app"]);
        assert_eq!(ancestors[0].1.workspaces, BUZZ_KEYS.workspaces);
        assert_eq!(ancestors[1].1.workspaces, SPROUT_KEYS.workspaces);
    }

    #[test]
    fn legacy_ancestors_of_waggle_dev_worktree_are_buzz_then_sprout_dev() {
        let ancestors = legacy_ancestors("xyz.waggle.app.dev.my-branch");
        let identifiers: Vec<&str> = ancestors.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            identifiers,
            [
                "xyz.block.buzz.app.dev.my-branch",
                "xyz.block.sprout.app.dev.my-branch"
            ]
        );
    }

    #[test]
    fn legacy_ancestors_of_a_still_current_buzz_identifier_is_sprout_only() {
        let ancestors = legacy_ancestors("xyz.block.buzz.app.dev.my-branch");
        let identifiers: Vec<&str> = ancestors.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(identifiers, ["xyz.block.sprout.app.dev.my-branch"]);
        assert_eq!(ancestors[0].1.workspaces, SPROUT_KEYS.workspaces);
    }

    #[test]
    fn legacy_ancestors_of_an_unrelated_identifier_is_empty() {
        assert!(legacy_ancestors("com.example.other").is_empty());
    }

    #[test]
    fn merging_an_older_ancestor_never_overwrites_a_nearer_one() {
        let mut result = LegacyWorkspaceStorage {
            workspaces: Some("buzz communities".to_string()),
            active_workspace_id: None,
            onboarding_completions: vec![LegacyOnboardingCompletion {
                pubkey: "buzz-user".to_string(),
                value: "true".to_string(),
            }],
        };
        let sprout = LegacyWorkspaceStorage {
            workspaces: Some("sprout workspaces".to_string()),
            active_workspace_id: Some("sprout-active".to_string()),
            onboarding_completions: vec![LegacyOnboardingCompletion {
                pubkey: "sprout-user".to_string(),
                value: "true".to_string(),
            }],
        };

        merge_legacy_workspace_storage(&mut result, sprout);

        // Buzz's workspaces survive; only the field Buzz left empty is filled.
        assert_eq!(result.workspaces.as_deref(), Some("buzz communities"));
        assert_eq!(result.active_workspace_id.as_deref(), Some("sprout-active"));
        // Onboarding completions from both generations are kept, not clobbered.
        assert_eq!(result.onboarding_completions.len(), 2);
    }

    #[test]
    fn decode_webkit_local_storage_value_reads_utf16le() {
        let bytes: Vec<u8> = "true".encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(
            decode_webkit_local_storage_value(&bytes).as_deref(),
            Some("true")
        );
    }

    #[test]
    fn decode_webkit_local_storage_value_reads_utf8_fallback() {
        assert_eq!(
            decode_webkit_local_storage_value(b"plain utf8").as_deref(),
            Some("plain utf8")
        );
    }

    #[test]
    fn read_legacy_workspace_storage_db_reads_workspace_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("localstorage.sqlite3");
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB NOT NULL ON CONFLICT FAIL)",
            [],
        )
        .unwrap();

        fn utf16le(value: &str) -> Vec<u8> {
            value.encode_utf16().flat_map(u16::to_le_bytes).collect()
        }

        conn.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
            (
                SPROUT_KEYS.workspaces,
                utf16le("[{\"relayUrl\":\"wss://relay.example.com\"}]"),
            ),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
            (SPROUT_KEYS.active_workspace, utf16le("workspace-1")),
        )
        .unwrap();
        conn.execute(
            "INSERT INTO ItemTable (key, value) VALUES (?1, ?2)",
            (
                format!("{}abc123", SPROUT_KEYS.onboarding_complete_prefix),
                utf16le("true"),
            ),
        )
        .unwrap();
        drop(conn);

        let storage = read_legacy_workspace_storage_db(&path, &SPROUT_KEYS).unwrap();
        assert_eq!(
            storage.workspaces.as_deref(),
            Some("[{\"relayUrl\":\"wss://relay.example.com\"}]")
        );
        assert_eq!(storage.active_workspace_id.as_deref(), Some("workspace-1"));
        assert_eq!(storage.onboarding_completions.len(), 1);
        assert_eq!(storage.onboarding_completions[0].pubkey, "abc123");
        assert_eq!(storage.onboarding_completions[0].value, "true");
    }
}
