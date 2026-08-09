//! Boot-time backfill of `community_relay_url` — the community an agent
//! instance belongs to — from the legacy creation-era `relay_url` pin.
//!
//! A non-empty pin is the best available evidence of the community the
//! identity was provisioned for; a blank pin carries no evidence, so the
//! record is left unscoped (`null` = offered in every community) rather than
//! guessed. Records created after this feature are stamped at creation and
//! never reach the backfill.
//!
//! Child module of `migration` so it reuses the parent's private JSON-patch
//! helpers (`patch_json_records`, `canonical_dev_data_dir`).

use std::path::Path;

use tauri::Manager as _;

use super::{canonical_dev_data_dir, patch_json_records};

/// Backfill `community_relay_url` onto every record that does not yet carry
/// the key (definitions included — an explicit `null` on a persona record is
/// correct and keeps the key-presence guard uniform).
///
/// Idempotent by **key presence**, not value: a record whose key holds an
/// explicit `null` was deliberately unscoped and is never re-derived from the
/// pin. This is also why the field serializes `null` instead of omitting it.
///
/// Rollback caveat: an older build drops the unknown key on its next store
/// write, so rolling back and forward re-runs this backfill — a user's
/// explicit unscope reverts to the pin-derived binding. Everything else
/// round-trips.
pub fn backfill_agent_community_scope(app: &tauri::AppHandle) {
    let Ok(current_dir) = app.path().app_data_dir() else {
        return;
    };
    let mut dirs = vec![current_dir.clone()];
    if let Some(canonical) = canonical_dev_data_dir(&current_dir) {
        if canonical.exists() && canonical != current_dir {
            dirs.push(canonical);
        }
    }
    for dir in dirs {
        let path = dir.join("agents/managed-agents.json");
        if path.exists() {
            backfill_community_scope_in_file(&path);
        }
    }
}

pub(super) fn backfill_community_scope_in_file(path: &Path) {
    patch_json_records(path, |obj| {
        if obj.contains_key("community_relay_url") {
            return false;
        }
        let pin = obj
            .get("relay_url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        // An unparsable pin degrades to null (unscoped/visible everywhere),
        // never to a garbage binding.
        let value = if pin.is_empty() {
            serde_json::Value::Null
        } else {
            buzz_core_pkg::relay::normalize_relay_url(pin)
                .map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null)
        };
        obj.insert("community_relay_url".to_string(), value);
        true
    });
}

#[cfg(test)]
mod tests {
    use super::backfill_community_scope_in_file;
    use crate::migration::test_support::{read_agents_json, write_agents_json};

    #[test]
    fn pinned_record_is_bound_to_its_pin() {
        let dir = tempfile::tempdir().unwrap();
        write_agents_json(
            dir.path(),
            &serde_json::json!([
                { "name": "Bumble", "pubkey": "aa", "relay_url": "wss://one.example" }
            ]),
        );
        backfill_community_scope_in_file(&dir.path().join("agents/managed-agents.json"));
        let records = read_agents_json(dir.path());
        assert_eq!(records[0]["community_relay_url"], "wss://one.example");
    }

    #[test]
    fn blank_pin_stays_unscoped_null() {
        // A blank pin carries no evidence of a home community; the record
        // must be visible everywhere, not bound by a guess.
        let dir = tempfile::tempdir().unwrap();
        write_agents_json(
            dir.path(),
            &serde_json::json!([
                { "name": "Alex", "pubkey": "bb", "relay_url": "" },
                { "name": "NoPin", "pubkey": "cc" }
            ]),
        );
        backfill_community_scope_in_file(&dir.path().join("agents/managed-agents.json"));
        let records = read_agents_json(dir.path());
        assert!(records[0]["community_relay_url"].is_null());
        assert!(records[0]
            .as_object()
            .unwrap()
            .contains_key("community_relay_url"));
        assert!(records[1]["community_relay_url"].is_null());
    }

    #[test]
    fn non_canonical_pin_is_stored_canonical() {
        let dir = tempfile::tempdir().unwrap();
        write_agents_json(
            dir.path(),
            &serde_json::json!([
                { "name": "Fizz", "pubkey": "dd", "relay_url": "WSS://One.Example:443/" }
            ]),
        );
        backfill_community_scope_in_file(&dir.path().join("agents/managed-agents.json"));
        let records = read_agents_json(dir.path());
        assert_eq!(records[0]["community_relay_url"], "wss://one.example");
    }

    #[test]
    fn unparsable_pin_degrades_to_null() {
        let dir = tempfile::tempdir().unwrap();
        write_agents_json(
            dir.path(),
            &serde_json::json!([
                { "name": "Odd", "pubkey": "ee", "relay_url": "https://not-a-relay.example" }
            ]),
        );
        backfill_community_scope_in_file(&dir.path().join("agents/managed-agents.json"));
        let records = read_agents_json(dir.path());
        assert!(records[0]["community_relay_url"].is_null());
    }

    #[test]
    fn explicit_null_and_existing_value_are_preserved() {
        // Key presence is the idempotency guard: an explicit null is a
        // deliberate unscope and must never be re-derived from the pin.
        let dir = tempfile::tempdir().unwrap();
        write_agents_json(
            dir.path(),
            &serde_json::json!([
                { "name": "Unscoped", "pubkey": "ff", "relay_url": "wss://one.example",
                  "community_relay_url": null },
                { "name": "Moved", "pubkey": "aa11", "relay_url": "wss://one.example",
                  "community_relay_url": "wss://two.example" }
            ]),
        );
        let path = dir.path().join("agents/managed-agents.json");
        let before = std::fs::read_to_string(&path).unwrap();
        backfill_community_scope_in_file(&path);
        let after = std::fs::read_to_string(&path).unwrap();
        assert_eq!(before, after, "records with the key present are untouched");
    }

    /// Manual verification hook: runs the real backfill against a COPY of a
    /// real store. Self-skips unless `BUZZ_COMMUNITY_SCOPE_MANUAL_DIR` names
    /// a directory containing `agents/managed-agents.json`. Mutates that
    /// copy in place and prints the resulting binding per record.
    #[test]
    fn manual_backfill_against_store_copy() {
        let Ok(dir) = std::env::var("BUZZ_COMMUNITY_SCOPE_MANUAL_DIR") else {
            return;
        };
        let path = std::path::Path::new(&dir).join("agents/managed-agents.json");
        assert!(path.exists(), "no store at {}", path.display());
        backfill_community_scope_in_file(&path);
        let first = std::fs::read_to_string(&path).unwrap();
        backfill_community_scope_in_file(&path);
        let second = std::fs::read_to_string(&path).unwrap();
        assert_eq!(first, second, "second run must be byte-identical");
        let records: Vec<serde_json::Value> = serde_json::from_str(&first).unwrap();
        for record in records.iter().filter(|r| {
            r.get("pubkey")
                .and_then(|p| p.as_str())
                .is_some_and(|p| !p.is_empty())
        }) {
            println!(
                "{}\t{}",
                record["name"].as_str().unwrap_or("?"),
                record["community_relay_url"]
            );
        }
    }

    #[test]
    fn second_run_is_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        write_agents_json(
            dir.path(),
            &serde_json::json!([
                { "name": "Bumble", "pubkey": "aa", "relay_url": "wss://one.example" },
                { "name": "Alex", "pubkey": "bb", "relay_url": "" }
            ]),
        );
        let path = dir.path().join("agents/managed-agents.json");
        backfill_community_scope_in_file(&path);
        let first = std::fs::read_to_string(&path).unwrap();
        backfill_community_scope_in_file(&path);
        let second = std::fs::read_to_string(&path).unwrap();
        assert_eq!(first, second, "second run must be a no-op");
    }
}
