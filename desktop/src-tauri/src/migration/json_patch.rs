//! Shared JSON-patch helper for boot migrations over the agent stores.

use std::path::Path;

/// Read a JSON array of objects from `path`, apply `f` to each object,
/// and write back if any mutation returned `true`.
///
/// Writes back via [`crate::managed_agents::atomic_write_json_restricted`]
/// (owner-only `0o600`): the store files this rewrites can carry plaintext
/// agent nsecs on a keyringless host, so the write must not reopen the umask
/// window SECURITY.md:90 closes.
pub(super) fn patch_json_records(
    path: &Path,
    mut f: impl FnMut(&mut serde_json::Map<String, serde_json::Value>) -> bool,
) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return;
    };
    let Ok(mut records) = serde_json::from_str::<Vec<serde_json::Value>>(&content) else {
        eprintln!(
            "buzz-desktop: patch-json-records: failed to parse {}",
            path.display()
        );
        return;
    };
    let mut changed = false;
    for record in &mut records {
        if let Some(obj) = record.as_object_mut() {
            changed |= f(obj);
        }
    }
    if changed {
        if let Ok(bytes) = serde_json::to_vec_pretty(&records) {
            if let Err(e) = crate::managed_agents::atomic_write_json_restricted(path, &bytes) {
                eprintln!("buzz-desktop: patch-json-records: {e}");
            }
        }
    }
}
