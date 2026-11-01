//! Small filesystem helpers: errors with the path, synced writes and directory `fsync`
//! (contract §6.1).

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use veta_core::{Result, VetaError};

/// I/O error that names the path it happened on.
pub(crate) fn io_err(path: &Path, err: io::Error) -> VetaError {
    VetaError::Io(format!("{}: {err}", path.display()))
}

/// Creates (or truncates) `path`, writes `bytes` and fsyncs the file.
pub(crate) fn write_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = File::create(path).map_err(|e| io_err(path, e))?;
    file.write_all(bytes).map_err(|e| io_err(path, e))?;
    file.sync_all().map_err(|e| io_err(path, e))
}

/// `fsync` of a directory, which makes the entries created or renamed in it durable.
pub(crate) fn sync_dir(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|dir| dir.sync_all())
        .map_err(|e| io_err(path, e))
}

/// `path` with `.tmp` appended to its file name.
pub(crate) fn tmp_sibling(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".tmp");
    PathBuf::from(name)
}

/// Replaces `path` atomically: writes `path.tmp`, fsyncs it, renames it over `path` and
/// fsyncs the directory.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = tmp_sibling(path);
    write_synced(&tmp, bytes)?;
    fs::rename(&tmp, path).map_err(|e| io_err(path, e))?;
    if let Some(parent) = path.parent() {
        sync_dir(parent)?;
    }
    Ok(())
}

/// Removes a directory tree; a tree that is already gone is not an error.
pub(crate) fn remove_dir_all_if_exists(path: &Path) -> Result<bool> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(io_err(path, e)),
    }
}

/// Names of the entries of a directory that are themselves directories, sorted.
pub(crate) fn subdirectories(path: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(path).map_err(|e| io_err(path, e))? {
        let entry = entry.map_err(|e| io_err(path, e))?;
        let is_dir = match entry.file_type() {
            Ok(t) => t.is_dir(),
            // The entry vanished between the listing and the stat.
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_err(&entry.path(), e)),
        };
        if is_dir && let Some(name) = entry.file_name().to_str() {
            names.push(name.to_string());
        }
    }
    names.sort();
    Ok(names)
}

#[cfg(test)]
pub(crate) mod testdir {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A fresh directory under the system temporary directory, removed on drop.
    pub struct TestDir(PathBuf);

    impl TestDir {
        pub fn new(tag: &str) -> TestDir {
            static N: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "veta-storage-{tag}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            TestDir(path)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testdir::TestDir;
    use super::*;

    #[test]
    fn atomic_write_replaces_and_leaves_no_tmp() {
        let dir = TestDir::new("fsutil");
        let path = dir.path().join("block_number.txt");
        write_atomic(&path, b"1\n").unwrap();
        write_atomic(&path, b"2\n").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"2\n");
        assert!(!tmp_sibling(&path).exists());
        assert_eq!(
            tmp_sibling(&path).file_name().unwrap(),
            "block_number.txt.tmp"
        );
    }

    #[test]
    fn subdirectories_skip_files_and_sort() {
        let dir = TestDir::new("fsutil-subdirs");
        fs::create_dir(dir.path().join("b")).unwrap();
        fs::create_dir(dir.path().join("a")).unwrap();
        fs::write(dir.path().join("file"), b"x").unwrap();
        assert_eq!(subdirectories(dir.path()).unwrap(), vec!["a", "b"]);
        assert!(remove_dir_all_if_exists(&dir.path().join("a")).unwrap());
        assert!(!remove_dir_all_if_exists(&dir.path().join("a")).unwrap());
    }
}
