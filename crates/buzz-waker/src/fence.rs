//! The shared write fence for the waker's durable state.
//!
//! Both durable stores — [`crate::floors`] and [`crate::cursor`] — are
//! read-modify-write against a file that another handle may be updating.
//! Neither may decide against a cached copy, and neither may write without
//! holding the fence. That discipline lives here once so it cannot be
//! half-applied: a review round on the floor store found three of its four
//! writers fenced and the fourth not, which is exactly the failure a shared,
//! witness-carrying primitive prevents.
//!
//! The lock is an exclusive advisory lock on a **sidecar** file
//! ([`std::fs::File::lock`] — `flock` on Unix, `LockFileEx` on Windows). It is
//! a sidecar rather than the record itself because the record is replaced by
//! `rename`, which swaps the inode out from under any lock held on it.

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// The sidecar path guarding `path`.
#[must_use]
pub(crate) fn lock_path_for(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    path.with_file_name(name)
}

/// An exclusive interprocess fence, released on drop.
///
/// Holding one is the *only* way to call [`atomic_write`], which takes a
/// `&FenceGuard` it never reads. The parameter is a compile-time witness:
/// writing outside the fence does not typecheck.
#[derive(Debug)]
pub(crate) struct FenceGuard {
    file: fs::File,
}

impl FenceGuard {
    /// Take the fence, creating the sidecar if needed.
    ///
    /// The sidecar carries no state, so creating it on demand is safe —
    /// whether the *record* may be absent is each store's own policy.
    pub(crate) fn acquire(lock_path: &Path) -> std::io::Result<Self> {
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(lock_path)?;
        file.lock()?;
        Ok(Self { file })
    }
}

impl Drop for FenceGuard {
    fn drop(&mut self) {
        // Best effort: closing the handle releases the lock regardless.
        let _ = self.file.unlock();
    }
}

/// Durably replace `path` with `bytes`: write-temp, fsync, rename, fsync-dir.
///
/// The directory fsync is what makes the rename itself durable — without it a
/// crash can leave the old contents visible even though the new file was
/// synced. It is POSIX-only and needs no platform gate here because the waker
/// runs on Linux; on Windows the call could only ever fail (`File::open` will
/// not open a directory without `FILE_FLAG_BACKUP_SEMANTICS`), which would
/// report failure *after* the record had already been replaced.
///
/// The temp name is process-unique so two writers cannot truncate each other's
/// staging file — though `_fence` is what actually orders them.
pub(crate) fn atomic_write(_fence: &FenceGuard, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));

    let mut file = fs::File::create(&tmp)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    drop(file);

    fs::rename(&tmp, path)?;

    #[cfg(unix)]
    {
        let dir = path.parent().unwrap_or_else(|| Path::new("."));
        fs::File::open(dir).and_then(|d| d.sync_all())?;
    }

    Ok(())
}
