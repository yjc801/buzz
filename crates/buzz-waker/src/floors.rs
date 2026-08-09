//! Durable anti-rollback floors — **G2**.
//!
//! A signature authenticates an *old* bundle exactly as well as a current one.
//! So "refuse a version lower than the one we hold" is only a real defence if
//! "hold" survives a restart: otherwise an attacker who can replay a
//! previously-valid signed bundle restarts the waker and gets the stale access
//! policy back — the precise regression the owner-only clamp exists to prevent.
//!
//! Two monotonic values, both persisted:
//!
//! - `highest_accepted_version` — the newest bundle ever activated;
//! - `revocation_floor` — the owner's published minimum acceptable version.
//!
//! # Ordering is the property
//!
//! [`FloorStore::admit`] persists **before** it returns `Ok`. A caller may
//! therefore treat a successful `admit` as "this version is durably recorded"
//! and activate the bundle. Persisting after activation would leave a crash
//! window in which the activated version is not on disk, which is the rollback
//! hole with extra steps.
//!
//! # Failing closed
//!
//! An unreadable or corrupt floor file is an error, never a reset to zero.
//! Silently defaulting would turn "someone deleted a byte" into "every old
//! bundle is acceptable again".

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Failures reading or advancing the durable floors.
#[derive(Debug, thiserror::Error)]
pub enum FloorError {
    /// The bundle's version is below the owner's published revocation floor.
    #[error("bundle version {version} is below the revocation floor {floor}")]
    Revoked {
        /// The rejected bundle's version.
        version: u64,
        /// The current revocation floor.
        floor: u64,
    },

    /// The bundle's version is older than one already activated — a rollback.
    #[error("bundle version {version} is older than the accepted {accepted}")]
    RolledBack {
        /// The rejected bundle's version.
        version: u64,
        /// The highest version already activated.
        accepted: u64,
    },

    /// The floor file exists but could not be read or parsed.
    ///
    /// Deliberately fatal: see the module note on failing closed.
    #[error("floor state at {path} is unreadable ({reason}); refusing to run without it")]
    Corrupt {
        /// The path that could not be read.
        path: String,
        /// Why.
        reason: String,
    },

    /// The floors could not be made durable.
    #[error("could not persist floor state to {path}: {reason}")]
    Persist {
        /// The path being written.
        path: String,
        /// Why.
        reason: String,
    },
}

/// The persisted monotonic state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Floors {
    /// Highest bundle version ever activated.
    pub highest_accepted_version: u64,
    /// Owner-published minimum acceptable bundle version.
    pub revocation_floor: u64,
}

/// A file-backed [`Floors`] with atomic, fail-closed updates.
#[derive(Debug)]
pub struct FloorStore {
    path: PathBuf,
    floors: Floors,
}

impl FloorStore {
    /// Load the floors, or start at zero if the file does not exist yet.
    ///
    /// # Errors
    /// [`FloorError::Corrupt`] if the file exists but cannot be read or parsed
    /// — a missing file is a first run, a broken one is not.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, FloorError> {
        let path = path.into();
        let floors = match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| FloorError::Corrupt {
                path: path.display().to_string(),
                reason: e.to_string(),
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Floors::default(),
            Err(e) => {
                return Err(FloorError::Corrupt {
                    path: path.display().to_string(),
                    reason: e.to_string(),
                })
            }
        };
        Ok(Self { path, floors })
    }

    /// The current floors.
    #[must_use]
    pub fn floors(&self) -> Floors {
        self.floors
    }

    /// Admit a bundle version, making the decision durable before returning.
    ///
    /// Re-admitting the version already accepted is a no-op success: bundles
    /// are re-delivered routinely and that is not a rollback.
    ///
    /// # Errors
    /// [`FloorError::Revoked`] or [`FloorError::RolledBack`] to refuse the
    /// bundle; [`FloorError::Persist`] if the new floor could not be made
    /// durable — in which case nothing is activated and in-memory state is
    /// left unchanged.
    pub fn admit(&mut self, version: u64) -> Result<(), FloorError> {
        if version < self.floors.revocation_floor {
            return Err(FloorError::Revoked {
                version,
                floor: self.floors.revocation_floor,
            });
        }
        if version < self.floors.highest_accepted_version {
            return Err(FloorError::RolledBack {
                version,
                accepted: self.floors.highest_accepted_version,
            });
        }
        if version == self.floors.highest_accepted_version {
            return Ok(());
        }

        let next = Floors {
            highest_accepted_version: version,
            ..self.floors
        };
        self.persist(next)?;
        self.floors = next;
        Ok(())
    }

    /// Raise the revocation floor. Monotonic by construction.
    ///
    /// Returns `true` when the floor moved. A floor at or below the current one
    /// is a stale or replayed revocation and is ignored rather than applied —
    /// lowering it would itself be the rollback.
    ///
    /// # Errors
    /// [`FloorError::Persist`] if the raised floor could not be made durable.
    pub fn raise_revocation_floor(&mut self, floor: u64) -> Result<bool, FloorError> {
        if floor <= self.floors.revocation_floor {
            return Ok(false);
        }
        let next = Floors {
            revocation_floor: floor,
            ..self.floors
        };
        self.persist(next)?;
        self.floors = next;
        Ok(true)
    }

    /// Write-temp, fsync, rename, fsync-dir.
    ///
    /// The directory fsync is what makes the rename itself durable; without it
    /// a crash can leave the old contents visible even though the new file was
    /// synced.
    fn persist(&self, next: Floors) -> Result<(), FloorError> {
        let fail = |reason: String| FloorError::Persist {
            path: self.path.display().to_string(),
            reason,
        };

        let encoded = serde_json::to_vec(&next).map_err(|e| fail(e.to_string()))?;
        let tmp = self.path.with_extension("tmp");

        let mut file = fs::File::create(&tmp).map_err(|e| fail(e.to_string()))?;
        file.write_all(&encoded).map_err(|e| fail(e.to_string()))?;
        file.sync_all().map_err(|e| fail(e.to_string()))?;
        drop(file);

        fs::rename(&tmp, &self.path).map_err(|e| fail(e.to_string()))?;

        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| fail(e.to_string()))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &tempfile::TempDir) -> FloorStore {
        FloorStore::open(dir.path().join("floors.json")).expect("open")
    }

    #[test]
    fn a_missing_file_starts_at_zero() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(store(&dir).floors(), Floors::default());
    }

    #[test]
    fn admitting_advances_the_floor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = store(&dir);
        s.admit(5).expect("admit 5");
        assert_eq!(s.floors().highest_accepted_version, 5);
    }

    /// G2, the headline case: accept an owner-only bundle, restart, then
    /// replay an older still-valid signed bundle. It must not be admitted.
    #[test]
    fn an_older_bundle_is_refused_after_a_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let mut s = store(&dir);
            s.admit(9).expect("admit 9");
        }
        // Fresh process, same disk.
        let mut reopened = store(&dir);
        assert_eq!(
            reopened.floors().highest_accepted_version,
            9,
            "the floor must survive the restart"
        );
        let err = reopened
            .admit(4)
            .expect_err("older version must be refused");
        assert!(matches!(
            err,
            FloorError::RolledBack {
                version: 4,
                accepted: 9
            }
        ));
    }

    #[test]
    fn re_admitting_the_current_version_is_a_no_op_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = store(&dir);
        s.admit(3).expect("admit 3");
        s.admit(3).expect("re-admit 3");
        assert_eq!(s.floors().highest_accepted_version, 3);
    }

    #[test]
    fn a_revoked_version_is_refused_even_when_it_is_newer_than_accepted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = store(&dir);
        s.admit(2).expect("admit 2");
        assert!(s.raise_revocation_floor(10).expect("raise"));
        let err = s.admit(5).expect_err("below the revocation floor");
        assert!(matches!(
            err,
            FloorError::Revoked {
                version: 5,
                floor: 10
            }
        ));
    }

    #[test]
    fn the_revocation_floor_never_decreases() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = store(&dir);
        assert!(s.raise_revocation_floor(7).expect("raise to 7"));
        assert!(
            !s.raise_revocation_floor(3).expect("stale revocation"),
            "a lower floor is ignored, not applied"
        );
        assert_eq!(s.floors().revocation_floor, 7);
    }

    #[test]
    fn the_revocation_floor_survives_a_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let mut s = store(&dir);
            s.raise_revocation_floor(12).expect("raise");
        }
        assert_eq!(store(&dir).floors().revocation_floor, 12);
    }

    /// Failing closed: a damaged floor file must not read as "no floor".
    #[test]
    fn a_corrupt_floor_file_refuses_to_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("floors.json");
        fs::write(&path, b"{not json").expect("write");
        let err = FloorStore::open(&path).expect_err("must refuse");
        assert!(matches!(err, FloorError::Corrupt { .. }));
    }

    #[test]
    fn no_temp_file_is_left_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = store(&dir);
        s.admit(1).expect("admit");
        assert!(!dir.path().join("floors.tmp").exists());
    }
}
