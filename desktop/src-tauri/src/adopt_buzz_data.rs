//! One-time adoption of the Buzz desktop app's data directory.
//!
//! This fork's desktop app is Waggle, which means a different Tauri bundle
//! identifier, which means a different `app_data_dir()`. Everything the app
//! owns — managed agents, personas, communities, channel templates, logs —
//! lives under the identifier, so renaming alone would present an existing
//! user with an app that looks freshly installed.
//!
//! Adoption **copies**, it never moves. Buzz must keep working: a user who
//! installs Waggle, dislikes it, and goes back should find their agents intact.
//! The cost is disk, which is cheap next to losing an agent's identity.
//!
//! The marker is written only after a fully successful copy, so a run that dies
//! partway retries from scratch on the next launch rather than leaving a
//! half-populated directory that looks adopted.

use std::fs;
use std::path::{Path, PathBuf};

/// Bundle identifier prefix this fork replaced.
const LEGACY_IDENTIFIER_PREFIX: &str = "xyz.block.buzz.app";
/// Bundle identifier prefix this fork uses.
const WAGGLE_IDENTIFIER_PREFIX: &str = "xyz.waggle.app";
/// Written into the destination once a copy completes in full.
const ADOPTION_MARKER: &str = ".adopted-from-buzz";

/// Boot entry point: adopt if needed, and report the outcome.
///
/// Deliberately infallible. A failed adoption must not stop the app from
/// starting — it starts empty instead, and because the marker is only written
/// on success the next launch tries again.
pub(crate) fn run_at_boot(data_dir: &Path) {
    match adopt_buzz_data_dir(data_dir) {
        Adoption::Copied { files } => {
            eprintln!("waggle: adopted {files} files from the Buzz data directory");
        }
        Adoption::Failed(error) => {
            eprintln!("waggle: could not adopt Buzz data, starting empty: {error}");
        }
        Adoption::Skipped(_) => {}
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Adoption {
    /// Nothing to do, with the reason — every skip is a normal outcome, not a
    /// failure, so callers log rather than surface these.
    Skipped(&'static str),
    Copied {
        files: usize,
    },
    Failed(String),
}

/// Map a Waggle data directory to the Buzz directory it should adopt from.
///
/// Prefix replacement rather than a fixed pair, so the dev identifier
/// (`…app.dev`) and per-worktree dev suffixes map to their Buzz counterparts
/// without enumerating them.
fn legacy_dir_for(data_dir: &Path) -> Option<PathBuf> {
    let name = data_dir.file_name()?.to_str()?;
    let suffix = name.strip_prefix(WAGGLE_IDENTIFIER_PREFIX)?;
    Some(data_dir.with_file_name(format!("{LEGACY_IDENTIFIER_PREFIX}{suffix}")))
}

/// Whether a directory holds anything worth preserving.
///
/// The marker itself does not count, and neither does an empty directory that
/// Tauri may have created just by resolving `app_data_dir()`.
fn has_content(dir: &Path) -> bool {
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries
        .flatten()
        .any(|entry| entry.file_name() != ADOPTION_MARKER)
}

/// Copy a directory tree. Returns the number of files written.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<usize> {
    fs::create_dir_all(to)?;
    let mut files = 0;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        // `file_type` rather than `metadata`, so a symlink is classified as a
        // link instead of whatever it points at — following one out of the
        // data directory would copy arbitrary parts of the filesystem.
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            files += copy_tree(&source, &target)?;
        } else if file_type.is_file() {
            fs::copy(&source, &target)?;
            files += 1;
        }
    }
    Ok(files)
}

/// Adopt the Buzz data directory into `data_dir`, once.
///
/// Safe to call on every boot: it is a no-op after the first success, and a
/// no-op for anyone who never ran Buzz.
pub(crate) fn adopt_buzz_data_dir(data_dir: &Path) -> Adoption {
    if data_dir.join(ADOPTION_MARKER).exists() {
        return Adoption::Skipped("already adopted");
    }
    // A populated destination means this install has its own history. Copying
    // Buzz over it would overwrite live state with something older.
    if has_content(data_dir) {
        return Adoption::Skipped("destination already has data");
    }
    let Some(legacy) = legacy_dir_for(data_dir) else {
        return Adoption::Skipped("data dir is not a Waggle identifier");
    };
    if !has_content(&legacy) {
        return Adoption::Skipped("no Buzz data to adopt");
    }

    match copy_tree(&legacy, data_dir) {
        Ok(files) => {
            // Marker last, and only on success: a failed copy must look
            // un-adopted so the next launch retries instead of starting from a
            // partial tree.
            if let Err(error) = fs::write(
                data_dir.join(ADOPTION_MARKER),
                legacy.to_string_lossy().as_bytes(),
            ) {
                return Adoption::Failed(format!(
                    "copied {files} files but could not write marker: {error}"
                ));
            }
            Adoption::Copied { files }
        }
        Err(error) => Adoption::Failed(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn seed(dir: &Path, rel: &str, body: &str) {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    fn dirs(tmp: &TempDir) -> (PathBuf, PathBuf) {
        let waggle = tmp.path().join(WAGGLE_IDENTIFIER_PREFIX);
        let buzz = tmp.path().join(LEGACY_IDENTIFIER_PREFIX);
        fs::create_dir_all(&waggle).unwrap();
        (waggle, buzz)
    }

    #[test]
    fn adopts_a_buzz_directory_into_an_empty_waggle_one() {
        let tmp = TempDir::new().unwrap();
        let (waggle, buzz) = dirs(&tmp);
        seed(&buzz, "agents/managed-agents.json", r#"[{"name":"Will"}]"#);
        seed(&buzz, "agents/logs/a.log", "log");

        assert_eq!(adopt_buzz_data_dir(&waggle), Adoption::Copied { files: 2 });
        assert_eq!(
            fs::read_to_string(waggle.join("agents/managed-agents.json")).unwrap(),
            r#"[{"name":"Will"}]"#
        );
    }

    #[test]
    fn leaves_the_buzz_directory_untouched() {
        // Copy, never move: going back to Buzz must find everything intact.
        let tmp = TempDir::new().unwrap();
        let (waggle, buzz) = dirs(&tmp);
        seed(&buzz, "agents/managed-agents.json", "original");

        adopt_buzz_data_dir(&waggle);
        assert_eq!(
            fs::read_to_string(buzz.join("agents/managed-agents.json")).unwrap(),
            "original"
        );
    }

    #[test]
    fn a_second_launch_does_not_copy_again() {
        let tmp = TempDir::new().unwrap();
        let (waggle, buzz) = dirs(&tmp);
        seed(&buzz, "agents/managed-agents.json", "from buzz");

        assert!(matches!(
            adopt_buzz_data_dir(&waggle),
            Adoption::Copied { .. }
        ));
        // Simulate the user editing state in Waggle after adoption.
        seed(&waggle, "agents/managed-agents.json", "edited in waggle");

        assert_eq!(
            adopt_buzz_data_dir(&waggle),
            Adoption::Skipped("already adopted")
        );
        // The re-run must not have clobbered the newer local edit.
        assert_eq!(
            fs::read_to_string(waggle.join("agents/managed-agents.json")).unwrap(),
            "edited in waggle"
        );
    }

    #[test]
    fn a_populated_destination_is_never_overwritten() {
        // No marker, but real data: an install with its own history must not be
        // buried under an older Buzz tree.
        let tmp = TempDir::new().unwrap();
        let (waggle, buzz) = dirs(&tmp);
        seed(&buzz, "agents/managed-agents.json", "from buzz");
        seed(&waggle, "agents/managed-agents.json", "already mine");

        assert_eq!(
            adopt_buzz_data_dir(&waggle),
            Adoption::Skipped("destination already has data")
        );
        assert_eq!(
            fs::read_to_string(waggle.join("agents/managed-agents.json")).unwrap(),
            "already mine"
        );
    }

    #[test]
    fn a_machine_that_never_ran_buzz_is_a_quiet_no_op() {
        let tmp = TempDir::new().unwrap();
        let (waggle, _buzz) = dirs(&tmp);
        assert_eq!(
            adopt_buzz_data_dir(&waggle),
            Adoption::Skipped("no Buzz data to adopt")
        );
        // No marker on a skip — if Buzz data appears later (restored backup),
        // the next launch should still adopt it.
        assert!(!waggle.join(ADOPTION_MARKER).exists());
    }

    #[test]
    fn the_dev_identifier_adopts_from_the_dev_buzz_directory() {
        // Prefix mapping, so `.dev` and per-worktree suffixes work without
        // being enumerated.
        let tmp = TempDir::new().unwrap();
        let waggle = tmp.path().join("xyz.waggle.app.dev");
        let buzz = tmp.path().join("xyz.block.buzz.app.dev");
        fs::create_dir_all(&waggle).unwrap();
        seed(&buzz, "settings.json", "{}");

        assert_eq!(adopt_buzz_data_dir(&waggle), Adoption::Copied { files: 1 });
    }

    #[test]
    fn an_unrelated_data_dir_is_left_alone() {
        let tmp = TempDir::new().unwrap();
        let other = tmp.path().join("com.example.other");
        fs::create_dir_all(&other).unwrap();
        assert_eq!(
            adopt_buzz_data_dir(&other),
            Adoption::Skipped("data dir is not a Waggle identifier")
        );
    }
}
