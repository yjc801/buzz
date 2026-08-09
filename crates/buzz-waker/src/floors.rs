//! The durable enrolment record and its anti-rollback floors — **G2**.
//!
//! A signature authenticates an *old* bundle exactly as well as a current one.
//! So "refuse a version lower than the one we hold" is only a real defence if
//! "hold" survives a restart: otherwise an attacker who can replay a
//! previously-valid signed bundle restarts the waker and gets the stale access
//! policy back — the precise regression the owner-only clamp exists to prevent.
//!
//! One record holds all three durable facts, because they are only meaningful
//! together:
//!
//! - `owner_pubkey` — pinned once, at enrolment, and never rewritten;
//! - `highest_accepted_version` — the newest bundle ever activated;
//! - `revocation_floor` — the owner's published minimum acceptable version.
//!
//! # Three ways this can be got wrong, and what stops each
//!
//! **Lost update.** Two `FloorStore` handles can each cache the record, and the
//! second one's write would clobber the first one's. Every mutation therefore
//! takes an exclusive advisory lock on a sidecar lock file
//! ([`std::fs::File::lock`], which is `flock` on Unix and `LockFileEx` on
//! Windows), **re-reads the record from disk under that lock**, decides
//! against those bytes, and writes before
//! releasing. The in-memory copy is a snapshot for display, never the basis of
//! a decision. The lock lives on a sidecar rather than on the record itself
//! because the record is replaced by `rename`, which swaps the inode out from
//! under any lock held on it.
//!
//! **Missing state.** A missing record is legitimate exactly once — at
//! enrolment. [`FloorStore::enroll`] creates it and refuses if one already
//! exists; [`FloorStore::open`] requires it and fails closed if it is gone.
//! Treating absence as "start at zero" would make deleting one file equivalent
//! to rolling every floor back.
//!
//! **Corruption.** An unreadable record is fatal for the same reason: silently
//! defaulting would turn "someone damaged a byte" into "every old bundle is
//! acceptable again".
//!
//! # Ordering
//!
//! [`FloorStore::admit`] persists **before** it returns `Ok`, so a caller may
//! treat success as "durably recorded" and activate the bundle. Persisting
//! after activation would leave a crash window in which the activated version
//! is not on disk — the rollback hole with extra steps.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Failures reading or advancing the durable record.
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

    /// No enrolment record where one is required.
    ///
    /// Deliberately fatal rather than an implicit re-enrolment: see the module
    /// note on missing state.
    #[error("no enrolment record at {path}; refusing to run (enroll explicitly to create one)")]
    NotEnrolled {
        /// The path that should have held the record.
        path: String,
    },

    /// Enrolment was asked to create a record that already exists.
    #[error("already enrolled at {path}; refusing to overwrite the durable floors")]
    AlreadyEnrolled {
        /// The path that already holds a record.
        path: String,
    },

    /// The record exists but could not be read or parsed.
    #[error("enrolment record at {path} is unreadable ({reason}); refusing to run without it")]
    Corrupt {
        /// The path that could not be read.
        path: String,
        /// Why.
        reason: String,
    },

    /// The record could not be made durable, or the fence could not be taken.
    #[error("could not persist enrolment record to {path}: {reason}")]
    Persist {
        /// The path being written.
        path: String,
        /// Why.
        reason: String,
    },
}

/// The persisted enrolment record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Floors {
    /// The owner whose signature a launch bundle must carry.
    ///
    /// Pinned at enrolment and never rewritten, so it cannot be swapped by any
    /// bundle that shows up later.
    pub owner_pubkey: String,
    /// Highest bundle version ever activated.
    pub highest_accepted_version: u64,
    /// Owner-published minimum acceptable bundle version.
    pub revocation_floor: u64,
}

/// A file-backed [`Floors`] with fenced, atomic, fail-closed updates.
#[derive(Debug)]
pub struct FloorStore {
    path: PathBuf,
    lock_path: PathBuf,
    /// Last-read snapshot. Never the basis of a decision — every mutation
    /// re-reads under the fence.
    snapshot: Floors,
}

impl FloorStore {
    fn lock_path_for(path: &Path) -> PathBuf {
        let mut name = path.file_name().unwrap_or_default().to_os_string();
        name.push(".lock");
        path.with_file_name(name)
    }

    /// Create the enrolment record, pinning `owner_pubkey`.
    ///
    /// Creation is fenced by the same sidecar lock as every other mutation,
    /// and absence is re-tested *under* it. An unfenced `exists()` check is a
    /// TOCTOU: two enrolments could both observe an empty store, the first
    /// could install its owner and advance the floor, and the second could
    /// then replace the record — resetting both floors and re-pinning the
    /// signer, defeating the very state the fence exists to protect.
    ///
    /// # Errors
    /// [`FloorError::AlreadyEnrolled`] if a record is already present — this
    /// never overwrites durable floors.
    pub fn enroll(path: impl Into<PathBuf>, owner_pubkey: &str) -> Result<Self, FloorError> {
        let path = path.into();
        let store = Self {
            lock_path: Self::lock_path_for(&path),
            path,
            snapshot: Floors {
                owner_pubkey: owner_pubkey.to_string(),
                highest_accepted_version: 0,
                revocation_floor: 0,
            },
        };

        let fence = FenceGuard::acquire(&store.lock_path, &store.path)?;
        if store.path.exists() {
            return Err(FloorError::AlreadyEnrolled {
                path: store.path.display().to_string(),
            });
        }
        let initial = store.snapshot.clone();
        store.persist(&fence, &initial)?;
        drop(fence);
        Ok(store)
    }

    /// Open an existing enrolment record.
    ///
    /// # Errors
    /// [`FloorError::NotEnrolled`] if the record is absent, or
    /// [`FloorError::Corrupt`] if it cannot be read — both fail closed rather
    /// than resetting the floors to zero.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, FloorError> {
        let path = path.into();
        let snapshot = Self::read(&path)?;
        Ok(Self {
            lock_path: Self::lock_path_for(&path),
            path,
            snapshot,
        })
    }

    /// The last-read record. A snapshot for display; decisions re-read.
    #[must_use]
    pub fn snapshot(&self) -> &Floors {
        &self.snapshot
    }

    /// The pinned owner, re-read from disk under the fence.
    ///
    /// # Errors
    /// Propagates a missing or corrupt record.
    pub fn pinned_owner(&mut self) -> Result<String, FloorError> {
        self.with_fence(|current| Ok((None, current.owner_pubkey.clone())))
    }

    /// Admit a bundle version, making the decision durable before returning.
    ///
    /// Re-admitting the version already accepted is a no-op success: bundles
    /// are re-delivered routinely and that is not a rollback.
    ///
    /// # Errors
    /// [`FloorError::Revoked`] or [`FloorError::RolledBack`] to refuse the
    /// bundle; [`FloorError::Persist`] if the new floor could not be made
    /// durable, in which case nothing is activated.
    pub fn admit(&mut self, version: u64) -> Result<(), FloorError> {
        self.with_fence(|current| {
            if version < current.revocation_floor {
                return Err(FloorError::Revoked {
                    version,
                    floor: current.revocation_floor,
                });
            }
            if version < current.highest_accepted_version {
                return Err(FloorError::RolledBack {
                    version,
                    accepted: current.highest_accepted_version,
                });
            }
            if version == current.highest_accepted_version {
                return Ok((None, ()));
            }
            let mut next = current.clone();
            next.highest_accepted_version = version;
            Ok((Some(next), ()))
        })
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
        self.with_fence(|current| {
            if floor <= current.revocation_floor {
                return Ok((None, false));
            }
            let mut next = current.clone();
            next.revocation_floor = floor;
            Ok((Some(next), true))
        })
    }

    /// Run a decision against the on-disk record under an exclusive fence.
    ///
    /// The closure receives the **freshly read** record and returns the record
    /// to persist (`None` for no change) plus its own result. Reading inside
    /// the fence is the whole point: a decision made against `self.snapshot`
    /// could be based on state another handle has already superseded, and
    /// writing it back would silently undo their update.
    fn with_fence<T>(
        &mut self,
        decide: impl FnOnce(&Floors) -> Result<(Option<Floors>, T), FloorError>,
    ) -> Result<T, FloorError> {
        let fence = FenceGuard::acquire(&self.lock_path, &self.path)?;
        let current = Self::read(&self.path)?;
        let (next, out) = decide(&current)?;
        if let Some(next) = next {
            self.persist(&fence, &next)?;
            self.snapshot = next;
        } else {
            self.snapshot = current;
        }
        drop(fence);
        Ok(out)
    }

    fn read(path: &Path) -> Result<Floors, FloorError> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| FloorError::Corrupt {
                path: path.display().to_string(),
                reason: e.to_string(),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(FloorError::NotEnrolled {
                path: path.display().to_string(),
            }),
            Err(e) => Err(FloorError::Corrupt {
                path: path.display().to_string(),
                reason: e.to_string(),
            }),
        }
    }

    /// Write-temp, fsync, rename, fsync-dir.
    ///
    /// The directory fsync is what makes the rename durable; without it a crash
    /// can leave the old contents visible even though the new file was synced.
    /// The temp name is process-unique so two writers cannot truncate each
    /// other's staging file — though the fence is what actually orders them.
    ///
    /// `_fence` is an unused witness parameter, and that is the point: it makes
    /// "never write outside the fence" a property the compiler enforces rather
    /// than one every future caller has to remember. The first version of this
    /// module fenced all three ordinary mutations and still let `enroll` write
    /// unfenced; a witness would have caught that at the type level.
    fn persist(&self, _fence: &FenceGuard, next: &Floors) -> Result<(), FloorError> {
        let fail = |reason: String| FloorError::Persist {
            path: self.path.display().to_string(),
            reason,
        };

        let encoded = serde_json::to_vec(next).map_err(|e| fail(e.to_string()))?;
        let tmp = self
            .path
            .with_extension(format!("tmp.{}", std::process::id()));

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

/// An exclusive interprocess fence over the record, released on drop.
struct FenceGuard {
    file: fs::File,
}

impl FenceGuard {
    fn acquire(lock_path: &Path, record_path: &Path) -> Result<Self, FloorError> {
        let fail = |reason: String| FloorError::Persist {
            path: record_path.display().to_string(),
            reason,
        };
        // The lock file carries no state, so creating it on demand is safe —
        // absence of the *record* is what fails closed, and `read` below
        // enforces that.
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path)
            .map_err(|e| fail(format!("could not open the fence: {e}")))?;
        file.lock()
            .map_err(|e| fail(format!("could not take the fence: {e}")))?;
        Ok(Self { file })
    }
}

impl Drop for FenceGuard {
    fn drop(&mut self) {
        // Best effort: closing the handle releases the lock regardless.
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWNER: &str = "ff00ff00";

    fn enrolled(dir: &tempfile::TempDir) -> FloorStore {
        FloorStore::enroll(dir.path().join("floors.json"), OWNER).expect("enroll")
    }

    fn reopen(dir: &tempfile::TempDir) -> FloorStore {
        FloorStore::open(dir.path().join("floors.json")).expect("open")
    }

    #[test]
    fn enrolment_pins_the_owner_and_starts_at_zero() {
        let dir = tempfile::tempdir().expect("tempdir");
        let s = enrolled(&dir);
        assert_eq!(s.snapshot().owner_pubkey, OWNER);
        assert_eq!(s.snapshot().highest_accepted_version, 0);
    }

    #[test]
    fn enrolment_refuses_to_overwrite_an_existing_record() {
        let dir = tempfile::tempdir().expect("tempdir");
        enrolled(&dir);
        let err =
            FloorStore::enroll(dir.path().join("floors.json"), "other").expect_err("must refuse");
        assert!(matches!(err, FloorError::AlreadyEnrolled { .. }));
    }

    /// A delayed second enrolment must not reset state the first has already
    /// advanced. Unfenced, B's `exists()` check could predate A's write, and B
    /// would then replace the record — re-pinning the signer and zeroing both
    /// floors.
    #[test]
    fn a_delayed_enrolment_cannot_reset_an_advanced_floor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("floors.json");

        let mut a = FloorStore::enroll(&path, "owner-x").expect("A enrols");
        a.admit(9).expect("A admits 9");

        let err = FloorStore::enroll(&path, "owner-y").expect_err("B must be refused");
        assert!(matches!(err, FloorError::AlreadyEnrolled { .. }));

        let after = FloorStore::open(&path).expect("open");
        assert_eq!(after.snapshot().owner_pubkey, "owner-x");
        assert_eq!(after.snapshot().highest_accepted_version, 9);
    }

    /// The actual race rather than its sequential shadow: several threads
    /// enrol the same path at once, each with a distinct owner. Exactly one
    /// may win and the durable record must be that winner's. Unfenced, more
    /// than one `exists()` check passes and the last writer wins.
    #[test]
    fn concurrent_enrolments_cannot_both_succeed() {
        use std::sync::Barrier;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("floors.json");
        const RACERS: usize = 8;
        let barrier = Barrier::new(RACERS);

        let outcomes: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..RACERS)
                .map(|i| {
                    let path = &path;
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        FloorStore::enroll(path, &format!("owner-{i}"))
                            .map(|s| s.snapshot().owner_pubkey.clone())
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("thread"))
                .collect()
        });

        let winners: Vec<_> = outcomes.iter().filter_map(|r| r.as_ref().ok()).collect();
        assert_eq!(
            winners.len(),
            1,
            "exactly one enrolment may win, got {winners:?}"
        );
        for outcome in outcomes.iter().filter(|r| r.is_err()) {
            let err = outcome.as_ref().expect_err("checked");
            assert!(
                matches!(err, FloorError::AlreadyEnrolled { .. }),
                "losers must be refused as already-enrolled, got {err:?}"
            );
        }
        assert_eq!(
            &FloorStore::open(&path)
                .expect("open")
                .snapshot()
                .owner_pubkey,
            winners[0],
            "the durable record must be the winner's"
        );
    }

    #[test]
    fn admitting_advances_the_floor() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = enrolled(&dir);
        s.admit(5).expect("admit 5");
        assert_eq!(s.snapshot().highest_accepted_version, 5);
    }

    /// G2, the headline case: accept an owner-only bundle, restart, then
    /// replay an older still-valid signed bundle. It must not be admitted.
    #[test]
    fn an_older_bundle_is_refused_after_a_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let mut s = enrolled(&dir);
            s.admit(9).expect("admit 9");
        }
        let mut reopened = reopen(&dir);
        assert_eq!(reopened.snapshot().highest_accepted_version, 9);
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

    /// Deleting the record must not read as "start at zero" — that would make
    /// `rm` a rollback primitive.
    #[test]
    fn a_deleted_record_fails_closed_instead_of_resetting() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("floors.json");
        {
            let mut s = enrolled(&dir);
            s.admit(9).expect("admit 9");
        }
        fs::remove_file(&path).expect("simulate loss");
        let err = FloorStore::open(&path).expect_err("must refuse");
        assert!(matches!(err, FloorError::NotEnrolled { .. }));
    }

    /// The lost-update race: two handles cache the same starting record, one
    /// advances the floor, and the other must not be able to write its stale
    /// view back over the top.
    #[test]
    fn a_stale_handle_cannot_clobber_a_newer_floor() {
        let dir = tempfile::tempdir().expect("tempdir");
        enrolled(&dir);
        let mut a = reopen(&dir);
        let mut b = reopen(&dir); // both snapshot version 0

        a.admit(9).expect("A admits 9");

        // B still believes the floor is 0. It must decide against disk, not
        // against its snapshot, and therefore refuse.
        assert_eq!(b.snapshot().highest_accepted_version, 0);
        let err = b.admit(4).expect_err("stale handle must not roll back");
        assert!(matches!(
            err,
            FloorError::RolledBack {
                version: 4,
                accepted: 9
            }
        ));
        assert_eq!(reopen(&dir).snapshot().highest_accepted_version, 9);
    }

    /// Same hazard across the two different fields: a revocation raised by one
    /// handle must not be erased by another handle's version admission.
    #[test]
    fn an_admit_does_not_erase_a_concurrently_raised_revocation_floor() {
        let dir = tempfile::tempdir().expect("tempdir");
        enrolled(&dir);
        let mut a = reopen(&dir);
        let mut b = reopen(&dir);

        a.raise_revocation_floor(20).expect("A raises to 20");
        b.admit(25).expect("B admits a version above the new floor");

        let final_state = reopen(&dir);
        assert_eq!(
            final_state.snapshot().revocation_floor,
            20,
            "B's write must have preserved A's revocation floor"
        );
        assert_eq!(final_state.snapshot().highest_accepted_version, 25);
    }

    #[test]
    fn re_admitting_the_current_version_is_a_no_op_success() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = enrolled(&dir);
        s.admit(3).expect("admit 3");
        s.admit(3).expect("re-admit 3");
        assert_eq!(s.snapshot().highest_accepted_version, 3);
    }

    #[test]
    fn a_revoked_version_is_refused_even_when_it_is_newer_than_accepted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = enrolled(&dir);
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
        let mut s = enrolled(&dir);
        assert!(s.raise_revocation_floor(7).expect("raise to 7"));
        assert!(
            !s.raise_revocation_floor(3).expect("stale revocation"),
            "a lower floor is ignored, not applied"
        );
        assert_eq!(s.snapshot().revocation_floor, 7);
    }

    #[test]
    fn the_revocation_floor_survives_a_restart() {
        let dir = tempfile::tempdir().expect("tempdir");
        {
            let mut s = enrolled(&dir);
            s.raise_revocation_floor(12).expect("raise");
        }
        assert_eq!(reopen(&dir).snapshot().revocation_floor, 12);
    }

    /// The pinned owner is enrolment state, not bundle state: no admission
    /// may rewrite it.
    #[test]
    fn admitting_never_rewrites_the_pinned_owner() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = enrolled(&dir);
        s.admit(4).expect("admit");
        s.raise_revocation_floor(2).expect("raise");
        assert_eq!(reopen(&dir).snapshot().owner_pubkey, OWNER);
    }

    #[test]
    fn a_corrupt_record_refuses_to_load() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("floors.json");
        fs::write(&path, b"{not json").expect("write");
        let err = FloorStore::open(&path).expect_err("must refuse");
        assert!(matches!(err, FloorError::Corrupt { .. }));
    }

    #[test]
    fn no_temp_file_is_left_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut s = enrolled(&dir);
        s.admit(1).expect("admit");
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .expect("read_dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    }
}
