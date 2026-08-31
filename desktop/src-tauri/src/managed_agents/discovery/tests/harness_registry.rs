use super::record_with;
use crate::managed_agents::discovery::{default_agent_command, try_record_agent_command};

// ── Registry lifecycle (C1) ───────────────────────────────────────────────────
//
// These tests verify the "warm → spawn resolves → delete → spawn errors" lifecycle
// that Paul's C1 ruling requires. They call the production resolution functions
// directly so they would red if warm_harness_registry_from_dir, save/delete
// transactional refresh, or try_record_agent_command were reverted.

/// After warm_harness_registry_from_dir, a record with a matching custom runtime
/// id resolves to the custom command — NOT the buzz-agent default.
///
/// This test would fail if warm_harness_registry_from_dir is not called before
/// try_record_agent_command, or if try_record_agent_command ignores the registry.
#[test]
fn registry_warm_then_try_record_resolves_custom_id() {
    use crate::managed_agents::custom_harnesses::{
        registry_test_lock, warm_harness_registry_from_dir,
    };
    use std::fs;
    use tempfile::tempdir;

    let _lock = registry_test_lock();
    let dir = tempdir().unwrap();
    fs::write(
        dir.path().join("my-custom-cli.json"),
        r#"{"id":"my-custom-cli","label":"My CLI","command":"my-custom-bin"}"#,
    )
    .unwrap();

    warm_harness_registry_from_dir(Some(dir.path()));

    let record = record_with(Some("my-custom-cli"), None, None);
    let result = try_record_agent_command(&record, &[]);
    assert_eq!(
        result,
        Ok("my-custom-bin".to_string()),
        "warm registry must make custom id resolvable at spawn time"
    );
}

/// After deleting a custom harness and re-warming the registry, a record that
/// still references the deleted id must produce a DANGLING_HARNESS_ID error —
/// NOT silently fall back to buzz-agent.
///
/// This test would fail if save/delete commands do not call
/// warm_harness_registry_from_dir transactionally, or if try_record_agent_command
/// silently falls back to default_agent_command() for dangling ids.
#[test]
fn registry_delete_then_try_record_returns_dangling_error() {
    use crate::managed_agents::custom_harnesses::{
        registry_test_lock, warm_harness_registry_from_dir,
    };
    use std::fs;
    use tempfile::tempdir;

    let _lock = registry_test_lock();
    let dir = tempdir().unwrap();
    let path = dir.path().join("soon-gone.json");
    fs::write(
        &path,
        r#"{"id":"soon-gone","label":"Gone","command":"soon-gone-bin"}"#,
    )
    .unwrap();
    warm_harness_registry_from_dir(Some(dir.path()));

    // Verify it resolves before delete.
    let record = record_with(Some("soon-gone"), None, None);
    assert!(
        try_record_agent_command(&record, &[]).is_ok(),
        "must resolve before delete"
    );

    // Simulate delete + re-warm (as delete_custom_harness does).
    fs::remove_file(&path).unwrap();
    warm_harness_registry_from_dir(Some(dir.path()));

    // Now must produce a typed error.
    let result = try_record_agent_command(&record, &[]);
    assert!(
        result.is_err(),
        "deleted id must produce Err after re-warm, got Ok({:?})",
        result.ok()
    );
    assert!(
        result.unwrap_err().contains("DANGLING_HARNESS_ID"),
        "error must contain DANGLING_HARNESS_ID"
    );
}

/// After saving (writing) a harness JSON and re-warming, the record resolves
/// to the new definition — simulating an immediate-save-then-start flow.
#[test]
fn registry_save_immediate_start_resolves_new_command() {
    use crate::managed_agents::custom_harnesses::{
        registry_test_lock, warm_harness_registry_from_dir,
    };
    use std::fs;
    use tempfile::tempdir;

    let _lock = registry_test_lock();
    let dir = tempdir().unwrap();

    // Before save: must not resolve.
    warm_harness_registry_from_dir(Some(dir.path()));
    let record = record_with(Some("fast-harness"), None, None);
    assert!(
        try_record_agent_command(&record, &[]).is_err(),
        "must not resolve before save"
    );

    // Simulate save + transactional re-warm.
    fs::write(
        dir.path().join("fast-harness.json"),
        r#"{"id":"fast-harness","label":"Fast","command":"fast-bin"}"#,
    )
    .unwrap();
    warm_harness_registry_from_dir(Some(dir.path()));

    let result = try_record_agent_command(&record, &[]);
    assert_eq!(
        result,
        Ok("fast-bin".to_string()),
        "immediate save+start must resolve without a discover_acp_providers round-trip"
    );
}

/// Editing a custom harness (renaming id + updating command) and re-warming the
/// registry makes both the old id a dangling reference and the new id resolvable.
#[test]
fn registry_edit_with_id_rename_old_dangling_new_resolved() {
    use crate::managed_agents::custom_harnesses::{
        registry_test_lock, warm_harness_registry_from_dir,
    };
    use std::fs;
    use tempfile::tempdir;

    // Serialize against all other tests that write to the global registry.
    let _lock = registry_test_lock();
    let dir = tempdir().unwrap();

    // Create original.
    fs::write(
        dir.path().join("original-id.json"),
        r#"{"id":"original-id","label":"Orig","command":"orig-bin"}"#,
    )
    .unwrap();
    warm_harness_registry_from_dir(Some(dir.path()));

    let old_record = record_with(Some("original-id"), None, None);
    assert!(
        try_record_agent_command(&old_record, &[]).is_ok(),
        "original id must resolve"
    );

    // Simulate rename: write new file, remove old file (same as save_custom_harness
    // with original_id set), then re-warm.
    fs::write(
        dir.path().join("renamed-id.json"),
        r#"{"id":"renamed-id","label":"Renamed","command":"new-bin"}"#,
    )
    .unwrap();
    fs::remove_file(dir.path().join("original-id.json")).unwrap();
    warm_harness_registry_from_dir(Some(dir.path()));

    // Old id must now be dangling.
    let result = try_record_agent_command(&old_record, &[]);
    assert!(
        result.is_err() && result.unwrap_err().contains("DANGLING_HARNESS_ID"),
        "original id must be dangling after rename"
    );

    // New id must resolve.
    let new_record = record_with(Some("renamed-id"), None, None);
    assert_eq!(
        try_record_agent_command(&new_record, &[]),
        Ok("new-bin".to_string()),
        "new id must resolve after rename"
    );
}

// ── Dangling-harness sentinel → user-facing surfaces ─────────────────────────

/// The internal `DANGLING_HARNESS_ID:<id>` sentinel round-trips through
/// `dangling_harness_id` and is never confused with other error strings.
#[test]
fn dangling_harness_id_extracts_id_and_rejects_other_errors() {
    use super::super::{dangling_harness_id, DANGLING_HARNESS_PREFIX};
    assert_eq!(
        dangling_harness_id(&format!("{DANGLING_HARNESS_PREFIX}my-harness")),
        Some("my-harness")
    );
    assert_eq!(dangling_harness_id("some other error"), None);
    assert_eq!(dangling_harness_id(""), None);
    // Prefix must anchor at the start — a wrapped sentinel is not a sentinel.
    assert_eq!(
        dangling_harness_id("cannot spawn: DANGLING_HARNESS_ID:x"),
        None
    );
}

/// Spawn surfaces the sentinel as an actionable sentence naming the id;
/// non-sentinel errors pass through untouched.
#[test]
fn user_facing_harness_error_converts_sentinel_to_sentence() {
    use super::super::{user_facing_harness_error, DANGLING_HARNESS_PREFIX};
    let msg = user_facing_harness_error(&format!("{DANGLING_HARNESS_PREFIX}foo"));
    assert!(
        msg.contains("\"foo\"") && msg.contains("deleted"),
        "sentence must name the missing harness, got: {msg}"
    );
    assert!(
        !msg.contains(DANGLING_HARNESS_PREFIX),
        "raw sentinel must never reach the user, got: {msg}"
    );
    assert_eq!(
        user_facing_harness_error("plain failure"),
        "plain failure",
        "non-sentinel errors pass through"
    );
}

/// Composed coherence test (delete → summary display → spawn sentence): after
/// a harness is deleted, the single resolver errors with the sentinel, the
/// summary path renders the *missing id* (not a silent buzz-agent fallback),
/// and the spawn path renders the actionable sentence — both halves tell the
/// same story from the same error.
#[test]
fn deleted_harness_summary_display_and_spawn_sentence_agree() {
    use crate::managed_agents::custom_harnesses::{
        delete_and_warm, registry_test_lock, save_and_warm,
    };
    use crate::managed_agents::{resolve_effective_harness_descriptor, GlobalAgentConfig};

    let _lock = registry_test_lock();
    let dir = tempfile::tempdir().unwrap();

    // Save a custom harness and pin a record to it.
    let def = crate::managed_agents::custom_harnesses::HarnessDefinition {
        id: "doomed".to_string(),
        label: "Doomed".to_string(),
        command: "doomed-bin".to_string(),
        args: vec![],
        env: Default::default(),
        install_instructions_url: String::new(),
        install_hint: String::new(),
    };
    save_and_warm(dir.path(), &def, None).unwrap();
    let record = record_with(Some("doomed"), None, None);
    let global = GlobalAgentConfig::default();
    assert!(
        resolve_effective_harness_descriptor(&record, &[], &global).is_ok(),
        "must resolve before delete"
    );

    // Delete it — the shared resolver used by BOTH spawn and summary errors.
    delete_and_warm(dir.path(), "doomed").unwrap();
    let err = resolve_effective_harness_descriptor(&record, &[], &global).unwrap_err();

    // Summary half: renders the missing id, never the default command.
    let id =
        super::super::dangling_harness_id(&err).expect("resolver must emit the typed sentinel");
    let display = super::super::dangling_harness_display(id);
    assert_eq!(display, "harness (deleted): doomed");
    assert!(!display.contains(&default_agent_command()));

    // Spawn half: renders a sentence naming the same id, no raw sentinel.
    let sentence = super::super::user_facing_harness_error(&err);
    assert!(sentence.contains("\"doomed\"") && sentence.contains("deleted"));
    assert!(!sentence.contains(super::super::DANGLING_HARNESS_PREFIX));
}

// ── I2: custom catalog entry carries definition_env for the edit round-trip ───

/// A custom harness definition that includes env vars must surface those vars
/// in the `definition_env` field of the resulting `AcpRuntimeCatalogEntry`.
///
/// This proves the edit-form round-trip: the backend carries env into the
/// catalog, the frontend reads it back when opening the edit form, and Save
/// therefore preserves existing env vars rather than silently erasing them.
#[test]
fn custom_catalog_entry_carries_definition_env_for_edit_roundtrip() {
    use crate::managed_agents::custom_harnesses::registry_test_lock;
    use crate::managed_agents::discovery::discover_acp_runtimes_from;
    use std::{collections::BTreeMap, fs};
    use tempfile::tempdir;

    // Discovery's auth probes read/warm the process-global PATH and
    // login-shell-PATH caches, and its final step publishes to the global
    // harness registry — hold both guards so parallel tests (e.g. the
    // PATH-swapping resolution tests) can't observe or absorb torn state.
    // Lock order for tests that need both: path lock first, then registry.
    let _path_guard = crate::managed_agents::lock_path_mutex();
    let _lock = registry_test_lock();
    let dir = tempdir().unwrap();
    // Write a custom definition with two env vars.
    fs::write(
        dir.path().join("env-harness.json"),
        r#"{
            "id": "env-harness",
            "label": "Env Harness",
            "command": "env-harness-bin",
            "args": [],
            "env": { "CURSOR_ACP": "1", "MY_TOKEN": "abc" }
        }"#,
    )
    .unwrap();

    let entries = discover_acp_runtimes_from(Some(dir.path()), true);
    let entry = entries
        .iter()
        .find(|e| e.id == "env-harness")
        .expect("custom entry must appear in catalog");

    let expected: BTreeMap<String, String> = [
        ("CURSOR_ACP".to_string(), "1".to_string()),
        ("MY_TOKEN".to_string(), "abc".to_string()),
    ]
    .into_iter()
    .collect();

    assert_eq!(
        entry.definition_env, expected,
        "catalog entry must carry definition env vars so the edit form can read them back"
    );
}

/// A builtin catalog entry must have an empty `definition_env` — their env
/// is handled via the `KnownAcpRuntime` metadata path, not user-editable JSON.
#[test]
fn builtin_catalog_entry_has_empty_definition_env() {
    use crate::managed_agents::custom_harnesses::registry_test_lock;
    use crate::managed_agents::discovery::discover_acp_runtimes_from;

    // Same guards as above: discovery probes PATH-dependent caches and
    // publishes to the global registry.
    let _path_guard = crate::managed_agents::lock_path_mutex();
    let _lock = registry_test_lock();
    let entries = discover_acp_runtimes_from(None, true);
    // Find any builtin entry (e.g. "goose" or "claude").
    let builtin = entries
        .iter()
        .find(|e| e.source == crate::managed_agents::HarnessSource::Builtin)
        .expect("at least one builtin must exist");

    assert!(
        builtin.definition_env.is_empty(),
        "builtin entry must not carry definition_env, got: {:?}",
        builtin.definition_env
    );
}

// ── Discovery publish via the PRODUCTION call path (stale-snapshot regression) ─
//
// These drive `discover_acp_runtimes_from` itself and land a save/delete in
// the window between its directory scan and its registry publish (via the
// `pre_publish_test_hook` seam). They red if discovery's final line reverts
// to publishing its pre-probe `loaded_defs` snapshot — the original bug —
// unlike the `custom_harnesses` seam tests, which pin only the fresh-read
// contract of `warm_harness_registry_locked`.

/// RAII guard: installs the pre-publish hook, clears it on drop (even on
/// panic) so a failing test cannot poison later ones.
struct PrePublishHookGuard;

impl PrePublishHookGuard {
    fn install(hook: Box<dyn Fn() + Send>) -> Self {
        super::super::pre_publish_test_hook::set(Some(hook));
        PrePublishHookGuard
    }
}

impl Drop for PrePublishHookGuard {
    fn drop(&mut self) {
        super::super::pre_publish_test_hook::set(None);
    }
}

fn harness_def(
    id: &str,
    label: &str,
    command: &str,
) -> crate::managed_agents::custom_harnesses::HarnessDefinition {
    crate::managed_agents::custom_harnesses::HarnessDefinition {
        id: id.to_string(),
        label: label.to_string(),
        command: command.to_string(),
        args: vec![],
        env: Default::default(),
        install_instructions_url: String::new(),
        install_hint: String::new(),
    }
}
/// A `save_and_warm` landing mid-discovery (after the scan, before the
/// publish) must survive discovery's registry publish — through the real
/// `discover_acp_runtimes_from` path.
#[test]
fn discovery_publish_path_survives_mid_flight_save() {
    use crate::managed_agents::custom_harnesses::{
        lookup_loaded_harness_by_id, registry_test_lock, save_and_warm,
    };
    use crate::managed_agents::discovery::discover_acp_runtimes_from;

    // Path lock first, then registry — discovery's probes touch the global
    // PATH caches (see the definition_env tests above for the full rationale).
    let _path_guard = crate::managed_agents::lock_path_mutex();
    let _lock = registry_test_lock();
    let dir = tempfile::tempdir().unwrap();

    // Discovery scans the dir while it is EMPTY; the save lands in the
    // pre-publish window. A stale-snapshot publish would clobber it.
    let hook_dir = dir.path().to_path_buf();
    let _guard = PrePublishHookGuard::install(Box::new(move || {
        let def = harness_def("mid-flight-save", "Mid Flight", "mid-flight-bin");
        save_and_warm(&hook_dir, &def, None).unwrap();
        assert!(lookup_loaded_harness_by_id("mid-flight-save").is_some());
    }));

    let _entries = discover_acp_runtimes_from(Some(dir.path()), true);

    assert!(
        lookup_loaded_harness_by_id("mid-flight-save").is_some(),
        "discovery's publish must re-read the directory — a stale-snapshot \
         publish clobbers a save that landed mid-discovery"
    );
}
/// A `delete_and_warm` landing mid-discovery must stay gone after discovery's
/// publish — a stale snapshot (taken while the file existed) would resurrect it.
#[test]
fn discovery_publish_path_drops_mid_flight_delete() {
    use crate::managed_agents::custom_harnesses::{
        delete_and_warm, lookup_loaded_harness_by_id, registry_test_lock, save_and_warm,
    };
    use crate::managed_agents::discovery::discover_acp_runtimes_from;

    // Path lock first, then registry (same rationale as the sibling test).
    let _path_guard = crate::managed_agents::lock_path_mutex();
    let _lock = registry_test_lock();
    let dir = tempfile::tempdir().unwrap();

    // File exists at scan time — discovery's snapshot would contain it.
    let def = harness_def("mid-flight-delete", "Mid Flight Del", "mid-flight-del-bin");
    save_and_warm(dir.path(), &def, None).unwrap();

    let hook_dir = dir.path().to_path_buf();
    let _guard = PrePublishHookGuard::install(Box::new(move || {
        delete_and_warm(&hook_dir, "mid-flight-delete").unwrap();
        assert!(lookup_loaded_harness_by_id("mid-flight-delete").is_none());
    }));

    let _entries = discover_acp_runtimes_from(Some(dir.path()), true);

    assert!(
        lookup_loaded_harness_by_id("mid-flight-delete").is_none(),
        "discovery's publish must not resurrect a harness deleted mid-discovery"
    );
}
