//! Recovery (contract §6.5) and detaching of broken parts (§6.4).
//!
//! (a) Every `tmp_*` whose `owner.lock` can be taken with `try_lock` (its owner died:
//!     `flock` is released by `kill -9`) and every `delete_tmp_*` is removed.
//! (b) Under an exclusive `.readers.lock`, the covered parts are removed (renamed to
//!     `delete_tmp_<name>` first, so a crash leaves only what step (a) cleans).
//! (c) Every active part is opened with the checks of §6.4; a broken one is renamed to
//!     `detached/broken_<name>` and reported.
//!
//! Before (b) deletes the parts a part covers, that covering part is verified, and a
//! broken one is detached then: otherwise a corrupt merge result would take with it the
//! only intact copy of its rows. A stray `owner.lock` left in a published part (an insert
//! that died right after its rename, see [`crate::insert`]) is removed in (c).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use veta_core::{Result, VetaError};

use crate::fsutil::{io_err, remove_dir_all_if_exists, subdirectories, sync_dir};
use crate::locks::{lock_exclusive, try_lock_exclusive};
use crate::part::{OWNER_LOCK, PartName, PartReader, active_parts, covered_parts, list_parts};
use crate::table::{DELETE_TMP_PREFIX, DETACHED_DIR, READERS_LOCK, TMP_PREFIX, Table};

/// A part that failed its integrity checks and was moved to `detached/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetachedPart {
    pub name: PartName,
    pub reason: String,
}

impl DetachedPart {
    /// The stderr line of §6.4: `veta: detached broken part <name>: <reason>`.
    pub fn message(&self) -> String {
        format!("veta: detached broken part {}: {}", self.name, self.reason)
    }
}

/// What [`recover`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// `tmp_*` directories of dead owners, removed.
    pub removed_tmp: Vec<String>,
    /// `tmp_*` directories whose owner is alive, left alone.
    pub kept_tmp: Vec<String>,
    pub removed_delete_tmp: Vec<String>,
    pub removed_covered: Vec<PartName>,
    pub detached: Vec<DetachedPart>,
}

/// Moves part `name` of the table in `table_dir` to `detached/broken_<name>` (with a
/// numeric suffix if that name is taken). `None` if the part was already gone.
pub fn detach_part(table_dir: &Path, name: PartName) -> Result<Option<PathBuf>> {
    let src = table_dir.join(name.to_string());
    let detached = table_dir.join(DETACHED_DIR);
    match fs::create_dir(&detached) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(io_err(&detached, e)),
    }
    let mut target = detached.join(format!("broken_{name}"));
    let mut n = 1;
    while target.exists() {
        target = detached.join(format!("broken_{name}_{n}"));
        n += 1;
    }
    match fs::rename(&src, &target) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound && !src.exists() => return Ok(None),
        Err(e) => return Err(io_err(&src, e)),
    }
    sync_dir(&detached)?;
    sync_dir(table_dir)?;
    Ok(Some(target))
}

/// Removes part `name` from the table in `table_dir`: renames it to `delete_tmp_<name>`, so
/// a crash leaves only what step (a) cleans, and deletes that. The caller holds the
/// exclusive `.readers.lock`. `false` if the part was already gone.
pub(crate) fn delete_covered_part(table_dir: &Path, name: PartName) -> Result<bool> {
    let src = table_dir.join(name.to_string());
    let doomed = table_dir.join(format!("{DELETE_TMP_PREFIX}{name}"));
    match fs::rename(&src, &doomed) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(io_err(&src, e)),
    }
    sync_dir(table_dir)?;
    remove_dir_all_if_exists(&doomed)?;
    Ok(true)
}

/// Opens `name` with every check of §6.4 (no column file kept open). `Ok(Some(reason))`
/// if it is broken, `Ok(None)` if it is fine or vanished.
fn verify_part(table: &Table, name: PartName) -> Result<Option<String>> {
    let dir = table.dir().join(name.to_string());
    match PartReader::open_columns(&dir, table.schema(), table.primary_key(), &[]) {
        Ok(_) => Ok(None),
        Err(_) if !dir.is_dir() => Ok(None),
        Err(VetaError::Corrupt { reason, .. }) => Ok(Some(reason)),
        Err(err) => Err(err),
    }
}

/// Detaches `name` if it is broken, recording it; whether it was.
fn detach_if_broken(table: &Table, name: PartName, report: &mut RecoveryReport) -> Result<bool> {
    match verify_part(table, name)? {
        Some(reason) => {
            detach_part(table.dir(), name)?;
            report.detached.push(DetachedPart { name, reason });
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Steps (a), (b) and (c) of §6.5 on `table`.
pub fn recover(table: &Table) -> Result<RecoveryReport> {
    let dir = table.dir();
    let mut report = RecoveryReport::default();

    // (a) Temporary directories of dead owners and interrupted deletions.
    for name in subdirectories(dir)? {
        let path = dir.join(&name);
        if name.starts_with(DELETE_TMP_PREFIX) {
            remove_dir_all_if_exists(&path)?;
            report.removed_delete_tmp.push(name);
        } else if name.starts_with(TMP_PREFIX) {
            match try_lock_exclusive(&path.join(OWNER_LOCK)) {
                Ok(Some(lock)) => {
                    remove_dir_all_if_exists(&path)?;
                    drop(lock);
                    report.removed_tmp.push(name);
                }
                Ok(None) => report.kept_tmp.push(name),
                // The directory vanished meanwhile: its owner published or removed it.
                Err(_) if !path.is_dir() => {}
                Err(err) => return Err(err),
            }
        }
    }
    sync_dir(dir)?;

    // (b) Covered parts, once the parts covering them are known to be intact.
    {
        let _readers = lock_exclusive(&table.lock_path(READERS_LOCK))?;
        let covered = loop {
            let parts = list_parts(dir)?;
            let covered = covered_parts(&parts);
            let mut detached_any = false;
            for coverer in active_parts(&parts) {
                if covered.iter().any(|c| coverer.covers(c))
                    && detach_if_broken(table, coverer, &mut report)?
                {
                    detached_any = true;
                }
            }
            if !detached_any {
                break covered;
            }
        };
        for name in covered {
            if delete_covered_part(dir, name)? {
                report.removed_covered.push(name);
            }
        }
        sync_dir(dir)?;
    }

    // (c) Integrity of every active part.
    for name in active_parts(&list_parts(dir)?) {
        if report.detached.iter().any(|d| d.name == name) {
            continue;
        }
        if detach_if_broken(table, name, &mut report)? {
            continue;
        }
        let stray = dir.join(name.to_string()).join(OWNER_LOCK);
        match fs::remove_file(&stray) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(&stray, e)),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::testdir::TestDir;
    use crate::insert::insert;
    use crate::part::tests::write_part;
    use crate::sort::sort_chunk;
    use crate::table::tests::sample_def;
    use veta_core::{Chunk, Value};

    fn batch(table: &Table, n: i64) -> Chunk {
        let rows: Vec<Vec<Value>> = (0..3)
            .map(|i| {
                vec![
                    Value::Int64(n * 10 + i),
                    Value::varchar(format!("s{}", (n + i) % 2)),
                    Value::Null,
                ]
            })
            .collect();
        Chunk::from_rows(&table.schema().data_types(), &rows).unwrap()
    }

    /// Writes the part `name` with the rows of batches `from..=to`, as a merge would.
    fn write_merged(table: &Table, name: PartName, from: i64, to: i64) {
        let mut rows = batch(table, from);
        for n in from + 1..=to {
            rows.extend(&batch(table, n)).unwrap();
        }
        let sorted = sort_chunk(&rows, table.primary_key())
            .unwrap()
            .unwrap_or(rows);
        write_part(
            table.dir(),
            name,
            table.schema(),
            table.primary_key(),
            &sorted,
            sorted.len,
        );
    }

    fn corrupt_part_txt(table: &Table, name: PartName) {
        let path = table.dir().join(name.to_string()).join("part.txt");
        let mut bytes = fs::read(&path).unwrap();
        bytes[0] ^= 0x01;
        fs::write(&path, bytes).unwrap();
    }

    #[test]
    fn removes_dead_tmp_dirs_and_respects_live_ones() {
        let db = TestDir::new("recover-tmp");
        let table = Table::create(db.path(), sample_def("t")).unwrap();
        insert(&table, &batch(&table, 1)).unwrap();

        let dead = table.dir().join("tmp_insert_9");
        fs::create_dir(&dead).unwrap();
        fs::write(dead.join(OWNER_LOCK), b"").unwrap();
        fs::write(dead.join("c0.bin"), b"partial").unwrap();
        let unlocked = table.dir().join("tmp_insert_10");
        fs::create_dir(&unlocked).unwrap();
        let live = table.dir().join("tmp_merge_all_1_2_1");
        fs::create_dir(&live).unwrap();
        let held = lock_exclusive(&live.join(OWNER_LOCK)).unwrap();
        let deleting = table.dir().join("delete_tmp_all_5_5_0");
        fs::create_dir(&deleting).unwrap();

        let report = recover(&table).unwrap();
        assert_eq!(report.removed_tmp, vec!["tmp_insert_10", "tmp_insert_9"]);
        assert_eq!(report.kept_tmp, vec!["tmp_merge_all_1_2_1"]);
        assert_eq!(report.removed_delete_tmp, vec!["delete_tmp_all_5_5_0"]);
        assert!(report.removed_covered.is_empty());
        assert!(report.detached.is_empty());
        assert!(!dead.exists() && !unlocked.exists() && !deleting.exists());
        assert!(live.join(OWNER_LOCK).exists());

        drop(held);
        let report = recover(&table).unwrap();
        assert_eq!(report.removed_tmp, vec!["tmp_merge_all_1_2_1"]);
        assert!(!live.exists());
        assert_eq!(
            subdirectories(table.dir()).unwrap(),
            vec!["all_1_1_0".to_string()]
        );
    }

    #[test]
    fn removes_covered_parts_and_stray_owner_locks() {
        let db = TestDir::new("recover-covered");
        let table = Table::create(db.path(), sample_def("t")).unwrap();
        for n in 1..=3 {
            insert(&table, &batch(&table, n)).unwrap();
        }
        let merged = PartName::new(1, 2, 1);
        write_merged(&table, merged, 1, 2);
        let stray = table.dir().join(merged.to_string()).join(OWNER_LOCK);
        fs::write(&stray, b"").unwrap();

        let report = recover(&table).unwrap();
        assert_eq!(
            report.removed_covered,
            vec![PartName::new(1, 1, 0), PartName::new(2, 2, 0)]
        );
        assert!(report.detached.is_empty());
        assert!(!stray.exists());
        assert_eq!(
            list_parts(table.dir()).unwrap(),
            vec![merged, PartName::new(3, 3, 0)]
        );
    }

    #[test]
    fn broken_parts_are_detached() {
        let db = TestDir::new("recover-broken");
        let table = Table::create(db.path(), sample_def("t")).unwrap();
        for n in 1..=3 {
            insert(&table, &batch(&table, n)).unwrap();
        }
        corrupt_part_txt(&table, PartName::new(3, 3, 0));
        let report = recover(&table).unwrap();
        assert_eq!(report.detached.len(), 1);
        assert_eq!(report.detached[0].name, PartName::new(3, 3, 0));
        assert!(
            report.detached[0]
                .message()
                .starts_with("veta: detached broken part all_3_3_0: ")
        );
        assert!(
            table
                .dir()
                .join(DETACHED_DIR)
                .join("broken_all_3_3_0")
                .is_dir()
        );
        assert_eq!(
            list_parts(table.dir()).unwrap(),
            vec![PartName::new(1, 1, 0), PartName::new(2, 2, 0)]
        );
    }

    #[test]
    fn a_broken_covering_part_does_not_take_the_covered_ones() {
        let db = TestDir::new("recover-broken-cover");
        let table = Table::create(db.path(), sample_def("t")).unwrap();
        for n in 1..=2 {
            insert(&table, &batch(&table, n)).unwrap();
        }
        let merged = PartName::new(1, 2, 1);
        write_merged(&table, merged, 1, 2);
        corrupt_part_txt(&table, merged);
        let report = recover(&table).unwrap();
        assert!(report.removed_covered.is_empty());
        assert_eq!(report.detached.len(), 1);
        assert_eq!(report.detached[0].name, merged);
        assert_eq!(
            list_parts(table.dir()).unwrap(),
            vec![PartName::new(1, 1, 0), PartName::new(2, 2, 0)]
        );
    }

    #[test]
    fn detaching_twice_keeps_both_copies() {
        let db = TestDir::new("recover-detach");
        let table = Table::create(db.path(), sample_def("t")).unwrap();
        let name = insert(&table, &batch(&table, 1)).unwrap().unwrap();
        let first = detach_part(table.dir(), name).unwrap().unwrap();
        assert!(first.ends_with("broken_all_1_1_0"));
        fs::create_dir(table.dir().join(name.to_string())).unwrap();
        let second = detach_part(table.dir(), name).unwrap().unwrap();
        assert!(second.ends_with("broken_all_1_1_0_1"));
        assert_eq!(detach_part(table.dir(), name).unwrap(), None);
    }
}
