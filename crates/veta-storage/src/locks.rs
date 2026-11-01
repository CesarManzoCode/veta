//! `flock` helpers on top of `std::fs::File::lock`, `lock_shared` and `try_lock`
//! (contract §6).
//!
//! A lock lives as long as its [`FileLock`]: dropping it closes the descriptor, which
//! releases the `flock`. The kernel also releases it when the process dies (`kill -9`
//! included), which is what lets recovery tell a dead owner from a live one (§6.5).
//! `flock` locks belong to the open file description, so two `File`s of the same process
//! on the same path conflict just as two processes do.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

use veta_core::Result;

use crate::fsutil::io_err;

/// A held `flock`; released on drop.
#[derive(Debug)]
pub struct FileLock {
    _file: File,
    path: PathBuf,
}

impl FileLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Opens (creating it if needed) the lock file at `path`.
fn open_lock_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| io_err(path, e))
}

/// Exclusive `flock` of `path`, waiting for it.
pub fn lock_exclusive(path: &Path) -> Result<FileLock> {
    let file = open_lock_file(path)?;
    file.lock().map_err(|e| io_err(path, e))?;
    Ok(FileLock {
        _file: file,
        path: path.to_path_buf(),
    })
}

/// Shared `flock` of `path`, waiting for it.
pub fn lock_shared(path: &Path) -> Result<FileLock> {
    let file = open_lock_file(path)?;
    file.lock_shared().map_err(|e| io_err(path, e))?;
    Ok(FileLock {
        _file: file,
        path: path.to_path_buf(),
    })
}

/// Exclusive `flock` of `path` without waiting: `None` if someone else holds it.
pub fn try_lock_exclusive(path: &Path) -> Result<Option<FileLock>> {
    let file = open_lock_file(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(FileLock {
            _file: file,
            path: path.to_path_buf(),
        })),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(io_err(path, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::testdir::TestDir;

    #[test]
    fn exclusive_lock_excludes_until_dropped() {
        let dir = TestDir::new("locks");
        let path = dir.path().join("owner.lock");
        let held = lock_exclusive(&path).unwrap();
        assert_eq!(held.path(), path);
        assert!(try_lock_exclusive(&path).unwrap().is_none());
        drop(held);
        let again = try_lock_exclusive(&path).unwrap();
        assert!(again.is_some());
    }

    #[test]
    fn shared_locks_coexist_and_block_exclusive() {
        let dir = TestDir::new("locks-shared");
        let path = dir.path().join(".readers.lock");
        let a = lock_shared(&path).unwrap();
        let b = lock_shared(&path).unwrap();
        assert!(try_lock_exclusive(&path).unwrap().is_none());
        drop(a);
        assert!(try_lock_exclusive(&path).unwrap().is_none());
        drop(b);
        assert!(try_lock_exclusive(&path).unwrap().is_some());
    }
}
