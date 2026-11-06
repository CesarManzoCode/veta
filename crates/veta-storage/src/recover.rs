//! Recovery (contract §6.5), `veta check` and detaching of broken parts (§6.4).
//!
//! (a) Every `tmp_*` whose `owner.lock` can be taken with `try_lock` (its owner died:
//!     `flock` is released by `kill -9`) and every `delete_tmp_*` is removed.
//! (b) Under an exclusive `.readers.lock`, the covered parts are removed (renamed to
//!     `delete_tmp_<name>` first, so a crash leaves only what step (a) cleans).
//! (c) Every active part is opened with the checks of §6.4; a broken one is renamed to
//!     `detached/broken_<name>` and reported.
//!
//! The states a crash leaves, and the active set each one recovers to:
//!
//! | Crash | On disk | Active set after recovery |
//! |---|---|---|
//! | Writing a part | `tmp_insert_<b>` or `tmp_merge_<name>`, whole or not | before |
//! | Published, before removing `owner.lock` | a stray `owner.lock` in the part | after |
//! | Merge published, before its cleanup | the part and all its sources | after |
//! | During the cleanup | the part, some sources, a `delete_tmp_*` whole or not | after |
//!
//! Beyond §6.5:
//!
//! - Step (a) runs under an exclusive `.insert.lock`, under which every `tmp_*` is created
//!   and locked ([`crate::insert::create_owned_tmp`]). Otherwise `try_lock` could succeed on
//!   a live directory between its `mkdir` and its `flock`.
//! - Before (b) deletes the parts a part covers, every block of that covering part is
//!   verified, and a broken one is detached then: otherwise a corrupt merge result would take
//!   with it the only intact copy of its rows. This runs outside `.readers.lock`, so readers
//!   do not wait for it, and only for the parts that cover something, which only a crash
//!   leaves behind.
//! - A stray `owner.lock` is removed in (c), and a `block_number.txt.tmp` in (a).

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use veta_core::{Result, VetaError};

use crate::fsutil::{io_err, remove_dir_all_if_exists, subdirectories, sync_dir, tmp_sibling};
use crate::locks::{lock_exclusive, lock_shared, try_lock_exclusive};
use crate::part::{OWNER_LOCK, PartName, PartReader, active_parts, covered_parts, list_parts};
use crate::table::{
    BLOCK_NUMBER_TXT, DELETE_TMP_PREFIX, DETACHED_DIR, INSERT_LOCK, READERS_LOCK, TMP_PREFIX, Table,
};

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

    /// The same finding as the error that ends the process with exit 3.
    pub fn error(&self) -> VetaError {
        VetaError::corrupt(self.name.to_string(), self.reason.clone())
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

/// What [`check_table`] found. Every part in `broken` and `corrupt` went to `detached/`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CheckReport {
    /// Parts looked at, whether or not they turned out corrupt.
    pub parts: u64,
    /// Blocks of `c<N>.bin` that verified.
    pub blocks: u64,
    /// Parts that failed the checks of opening.
    pub broken: Vec<DetachedPart>,
    /// Parts that opened and have a block that fails its checks.
    pub corrupt: Vec<DetachedPart>,
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

/// What a reader does when a block fails (§6.4): if `err` is corruption, the part it names
/// goes to `detached/`. Any other error leaves everything in place.
pub fn detach_corrupt(table_dir: &Path, err: &VetaError) -> Result<()> {
    if let VetaError::Corrupt { part, .. } = err
        && let Some(name) = PartName::parse(part)
    {
        detach_part(table_dir, name)?;
    }
    Ok(())
}

/// Removes part `name` from the table in `table_dir`: renames it to `delete_tmp_<name>`, so
/// a crash leaves only what step (a) cleans, and deletes that. The caller holds the
/// exclusive `.readers.lock`. `false` if the part was already gone.
pub(crate) fn delete_covered_part(table_dir: &Path, name: PartName) -> Result<bool> {
    let src = table_dir.join(name.to_string());
    let doomed = table_dir.join(format!("{DELETE_TMP_PREFIX}{name}"));
    // A crash may have left one of that name, and a rename cannot go over it.
    remove_dir_all_if_exists(&doomed)?;
    match fs::rename(&src, &doomed) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(io_err(&src, e)),
    }
    sync_dir(table_dir)?;
    remove_dir_all_if_exists(&doomed)?;
    Ok(true)
}

/// The reason of a failed integrity check of the part in `dir`: `None` if `checked` passed or
/// the part vanished meanwhile.
fn broken_reason<T>(checked: Result<T>, dir: &Path) -> Result<Option<String>> {
    match checked {
        Ok(_) => Ok(None),
        Err(_) if !dir.is_dir() => Ok(None),
        Err(VetaError::Corrupt { reason, .. }) => Ok(Some(reason)),
        Err(err) => Err(err),
    }
}

/// Opens `name` with every check of §6.4 (no column file kept open).
fn verify_part(table: &Table, name: PartName) -> Result<Option<String>> {
    let dir = table.dir().join(name.to_string());
    let opened = PartReader::open_columns(&dir, table.schema(), table.primary_key(), &[]);
    broken_reason(opened, &dir)
}

/// [`verify_part`] and, on top of it, every block of every column.
fn verify_part_and_blocks(table: &Table, name: PartName) -> Result<Option<String>> {
    let dir = table.dir().join(name.to_string());
    let checked = PartReader::open(&dir, table.schema(), table.primary_key())
        .and_then(|part| part.verify_blocks());
    broken_reason(checked, &dir)
}

/// Detaches `name` if `reason` says it is broken, recording it; whether it was.
fn detach_if_broken(
    table: &Table,
    name: PartName,
    reason: Option<String>,
    report: &mut RecoveryReport,
) -> Result<bool> {
    match reason {
        Some(reason) => {
            detach_part(table.dir(), name)?;
            report.detached.push(DetachedPart { name, reason });
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Step (a): temporary directories of dead owners and interrupted deletions.
fn remove_abandoned(table: &Table, report: &mut RecoveryReport) -> Result<()> {
    let dir = table.dir();
    let _creating = lock_exclusive(&table.lock_path(INSERT_LOCK))?;
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
    // Only whoever holds `.insert.lock` writes this file, so one that is here is abandoned.
    let stale = tmp_sibling(&dir.join(BLOCK_NUMBER_TXT));
    match fs::remove_file(&stale) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(io_err(&stale, e)),
    }
    sync_dir(dir)
}

/// Step (b): covered parts, once the parts covering them are known to be intact. A part that
/// only a broken part covered is active again, and may cover others in turn.
fn remove_covered(table: &Table, report: &mut RecoveryReport) -> Result<()> {
    let dir = table.dir();
    let mut verified: Vec<PartName> = Vec::new();
    loop {
        let parts = list_parts(dir)?;
        let covered = covered_parts(&parts);
        let unverified: Vec<PartName> = active_parts(&parts)
            .into_iter()
            .filter(|coverer| !verified.contains(coverer))
            .filter(|coverer| covered.iter().any(|part| coverer.covers(part)))
            .collect();
        if unverified.is_empty() {
            break;
        }
        for coverer in unverified {
            let reason = verify_part_and_blocks(table, coverer)?;
            if !detach_if_broken(table, coverer, reason, report)? {
                verified.push(coverer);
            }
        }
    }

    let _readers = lock_exclusive(&table.lock_path(READERS_LOCK))?;
    let parts = list_parts(dir)?;
    for name in covered_parts(&parts) {
        // Whatever else covers it is a merge still running, which cleans up after itself.
        let safe = verified
            .iter()
            .any(|coverer| coverer.covers(&name) && parts.contains(coverer));
        if safe && delete_covered_part(dir, name)? {
            report.removed_covered.push(name);
        }
    }
    sync_dir(dir)
}

/// Steps (a), (b) and (c) of §6.5 on `table`.
pub fn recover(table: &Table) -> Result<RecoveryReport> {
    let dir = table.dir();
    let mut report = RecoveryReport::default();
    remove_abandoned(table, &mut report)?;
    remove_covered(table, &mut report)?;

    // (c) Integrity of every active part.
    for name in active_parts(&list_parts(dir)?) {
        let reason = verify_part(table, name)?;
        if detach_if_broken(table, name, reason, &mut report)? {
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

/// Detaches a part `check` found corrupt. A failure to do so does not change what was found,
/// so it goes in the reason: the check must still end with exit 3.
fn detached_by_check(table_dir: &Path, name: PartName, reason: String) -> DetachedPart {
    let reason = match detach_part(table_dir, name) {
        Ok(_) => reason,
        Err(err) => format!("{reason} (it could not be detached: {err})"),
    };
    DetachedPart { name, reason }
}

/// `veta check` (§6.4): verifies every block of every part present, covered ones included,
/// and detaches the corrupt ones. One part is open at a time, so no table is too wide for the
/// limit of open files, and it looks again at the end for parts a merge published meanwhile.
pub fn check_table(table: &Table) -> Result<CheckReport> {
    let dir = table.dir();
    let mut report = CheckReport::default();
    let mut seen = HashSet::new();
    loop {
        let pending: Vec<PartName> = list_parts(dir)?
            .into_iter()
            .filter(|part| !seen.contains(part))
            .collect();
        if pending.is_empty() {
            return Ok(report);
        }
        for name in pending {
            seen.insert(name);
            let path = dir.join(name.to_string());
            let opened = {
                let _readers = lock_shared(&table.lock_path(READERS_LOCK))?;
                PartReader::open(&path, table.schema(), table.primary_key())
            };
            let part = match opened {
                Ok(part) => part,
                Err(_) if !path.is_dir() => continue,
                Err(VetaError::Corrupt { reason, .. }) => {
                    report.parts += 1;
                    report.broken.push(detached_by_check(dir, name, reason));
                    continue;
                }
                Err(err) => return Err(err),
            };
            report.parts += 1;
            match part.verify_blocks() {
                Ok(blocks) => report.blocks += blocks,
                Err(VetaError::Corrupt { reason, .. }) => {
                    report.corrupt.push(detached_by_check(dir, name, reason));
                }
                Err(err) => return Err(err),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::testdir::TestDir;
    use crate::insert::insert;
    use crate::part::PartWriter;
    use crate::part::tests::write_part;
    use crate::snapshot::Snapshot;
    use crate::sort::sort_chunk;
    use crate::table::tests::sample_def;
    use std::thread;
    use std::time::Duration;
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

    /// The rows of the active parts, sorted.
    fn table_rows(table: &Table) -> Vec<Vec<Value>> {
        let snapshot = Snapshot::take(table, None).unwrap();
        assert!(snapshot.detached().is_empty());
        let mut rows = Vec::new();
        for part in snapshot.parts() {
            let columns: Vec<_> = (0..table.schema().len())
                .map(|c| part.read_column(c).unwrap())
                .collect();
            for row in 0..part.rows() as usize {
                rows.push(columns.iter().map(|c| c.get(row)).collect::<Vec<Value>>());
            }
        }
        rows.sort();
        rows
    }

    fn assert_no_temporaries(table: &Table) {
        for name in subdirectories(table.dir()).unwrap() {
            assert!(
                !name.starts_with(TMP_PREFIX) && !name.starts_with(DELETE_TMP_PREFIX),
                "{name} is still there"
            );
        }
    }

    /// A table with parts 1, 2 and 3, and its rows.
    fn three_parts(db: &TestDir) -> (Table, Vec<Vec<Value>>) {
        let table = Table::create(db.path(), sample_def("t")).unwrap();
        for n in 1..=3 {
            insert(&table, &batch(&table, n)).unwrap();
        }
        let rows = table_rows(&table);
        (table, rows)
    }

    fn sources_and_rest() -> Vec<PartName> {
        vec![
            PartName::new(1, 1, 0),
            PartName::new(2, 2, 0),
            PartName::new(3, 3, 0),
        ]
    }

    fn flip_byte(table: &Table, name: PartName, file: &str) {
        let path = table.dir().join(name.to_string()).join(file);
        let mut bytes = fs::read(&path).unwrap();
        let middle = bytes.len() / 2;
        bytes[middle] ^= 0x55;
        fs::write(&path, bytes).unwrap();
    }

    #[test]
    fn cut_while_inserting_recovers_to_before() {
        let db = TestDir::new("recover-cut-insert");
        let (table, before) = three_parts(&db);
        let name = PartName::new(4, 4, 0);
        let tmp = table.dir().join("tmp_insert_4");
        fs::create_dir(&tmp).unwrap();
        // The owner died (its lock is free) with some rows in the buffers and files.
        drop(lock_exclusive(&tmp.join(OWNER_LOCK)).unwrap());
        let mut writer =
            PartWriter::create(&tmp, name, table.schema(), table.primary_key(), 10).unwrap();
        let rows = batch(&table, 4);
        writer
            .write(
                &sort_chunk(&rows, table.primary_key())
                    .unwrap()
                    .unwrap_or(rows),
            )
            .unwrap();
        drop(writer);
        // And it may have died while rewriting the watermark.
        let stale = table.dir().join("block_number.txt.tmp");
        fs::write(&stale, b"4\n").unwrap();

        let report = recover(&table).unwrap();
        assert_eq!(report.removed_tmp, vec!["tmp_insert_4"]);
        assert!(!stale.exists());
        assert_no_temporaries(&table);
        assert_eq!(list_parts(table.dir()).unwrap(), sources_and_rest());
        assert_eq!(table_rows(&table), before);
        // The dead insert had moved the watermark, so its block is not given out again.
        fs::write(table.dir().join(BLOCK_NUMBER_TXT), b"4\n").unwrap();
        assert_eq!(
            insert(&table, &batch(&table, 5)).unwrap(),
            Some(PartName::new(5, 5, 0))
        );
    }

    #[test]
    fn cut_with_the_merge_written_and_not_renamed_recovers_to_before() {
        let db = TestDir::new("recover-cut-merge-tmp");
        let (table, before) = three_parts(&db);
        let merged = PartName::new(1, 2, 1);
        write_merged(&table, merged, 1, 2);
        let tmp = table.dir().join(format!("tmp_merge_{merged}"));
        fs::rename(table.dir().join(merged.to_string()), &tmp).unwrap();
        fs::write(tmp.join(OWNER_LOCK), b"").unwrap();

        let report = recover(&table).unwrap();
        assert_eq!(report.removed_tmp, vec!["tmp_merge_all_1_2_1"]);
        assert!(report.removed_covered.is_empty());
        assert_no_temporaries(&table);
        assert_eq!(list_parts(table.dir()).unwrap(), sources_and_rest());
        assert_eq!(table_rows(&table), before);
    }

    #[test]
    fn cut_with_the_merge_renamed_and_its_sources_present_recovers_to_after() {
        let db = TestDir::new("recover-cut-merge-renamed");
        let (table, before) = three_parts(&db);
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
        assert_no_temporaries(&table);
        assert_eq!(
            list_parts(table.dir()).unwrap(),
            vec![merged, PartName::new(3, 3, 0)]
        );
        assert_eq!(table_rows(&table), before);
    }

    #[test]
    fn cut_during_the_cleanup_recovers_to_after() {
        let db = TestDir::new("recover-cut-delete");
        let (table, before) = three_parts(&db);
        let merged = PartName::new(1, 2, 1);
        write_merged(&table, merged, 1, 2);
        // One source half deleted; the other one not reached yet.
        let doomed = table.dir().join("delete_tmp_all_1_1_0");
        fs::rename(table.dir().join("all_1_1_0"), &doomed).unwrap();
        fs::remove_file(doomed.join("part.txt")).unwrap();
        fs::remove_file(doomed.join("c0.bin")).unwrap();

        // Before recovering, a reader already sees the same rows.
        assert_eq!(table_rows(&table), before);
        let report = recover(&table).unwrap();
        assert_eq!(report.removed_delete_tmp, vec!["delete_tmp_all_1_1_0"]);
        assert_eq!(report.removed_covered, vec![PartName::new(2, 2, 0)]);
        assert_no_temporaries(&table);
        assert_eq!(
            list_parts(table.dir()).unwrap(),
            vec![merged, PartName::new(3, 3, 0)]
        );
        assert_eq!(table_rows(&table), before);
    }

    #[test]
    fn recovering_twice_changes_nothing() {
        let db = TestDir::new("recover-twice");
        let (table, before) = three_parts(&db);
        write_merged(&table, PartName::new(1, 2, 1), 1, 2);
        recover(&table).unwrap();
        let parts = list_parts(table.dir()).unwrap();
        assert_eq!(recover(&table).unwrap(), RecoveryReport::default());
        assert_eq!(list_parts(table.dir()).unwrap(), parts);
        assert_eq!(table_rows(&table), before);
    }

    #[test]
    fn a_corrupt_block_in_a_covering_part_keeps_the_covered_ones() {
        let db = TestDir::new("recover-cover-block");
        let (table, before) = three_parts(&db);
        let merged = PartName::new(1, 2, 1);
        write_merged(&table, merged, 1, 2);
        flip_byte(&table, merged, "c0.bin");

        let report = recover(&table).unwrap();
        assert!(report.removed_covered.is_empty());
        assert_eq!(report.detached.len(), 1);
        assert_eq!(report.detached[0].name, merged);
        assert_eq!(list_parts(table.dir()).unwrap(), sources_and_rest());
        assert_eq!(table_rows(&table), before);
    }

    #[test]
    fn a_broken_covering_part_leaves_the_next_level_to_decide() {
        let db = TestDir::new("recover-cover-levels");
        let (table, before) = three_parts(&db);
        let (inner, outer) = (PartName::new(1, 2, 1), PartName::new(1, 3, 2));
        write_merged(&table, inner, 1, 2);
        write_merged(&table, outer, 1, 3);
        flip_byte(&table, outer, "stats.bin");

        let report = recover(&table).unwrap();
        assert_eq!(report.detached.len(), 1);
        assert_eq!(report.detached[0].name, outer);
        assert_eq!(
            report.removed_covered,
            vec![PartName::new(1, 1, 0), PartName::new(2, 2, 0)]
        );
        assert_eq!(
            list_parts(table.dir()).unwrap(),
            vec![inner, PartName::new(3, 3, 0)]
        );
        assert_eq!(table_rows(&table), before);
    }

    #[test]
    fn parts_with_the_same_range_are_not_deleted() {
        let db = TestDir::new("recover-twins");
        let table = Table::create(db.path(), sample_def("t")).unwrap();
        let twins = vec![PartName::new(1, 2, 1), PartName::new(1, 2, 2)];
        for &twin in &twins {
            write_merged(&table, twin, 1, 2);
        }
        let report = recover(&table).unwrap();
        assert!(report.removed_covered.is_empty());
        assert_eq!(list_parts(table.dir()).unwrap(), twins);
    }

    #[test]
    fn a_live_tmp_between_its_mkdir_and_its_flock_is_not_taken_for_dead() {
        let db = TestDir::new("recover-window");
        let table = Table::create(db.path(), sample_def("t")).unwrap();
        // Where an insert or a merge stands between creating the directory and locking it.
        let creating = lock_exclusive(&table.lock_path(INSERT_LOCK)).unwrap();
        let tmp = table.dir().join("tmp_insert_5");
        fs::create_dir(&tmp).unwrap();
        thread::scope(|scope| {
            let recovering = scope.spawn(|| recover(&table).unwrap());
            thread::sleep(Duration::from_millis(100));
            assert!(tmp.is_dir());
            let owner = lock_exclusive(&tmp.join(OWNER_LOCK)).unwrap();
            drop(creating);
            let report = recovering.join().unwrap();
            assert_eq!(report.kept_tmp, vec!["tmp_insert_5"]);
            assert!(report.removed_tmp.is_empty());
            assert!(tmp.join(OWNER_LOCK).exists());
            drop(owner);
        });
        let report = recover(&table).unwrap();
        assert_eq!(report.removed_tmp, vec!["tmp_insert_5"]);
        assert_no_temporaries(&table);
    }

    #[test]
    fn a_leftover_deletion_does_not_stop_a_new_one() {
        let db = TestDir::new("recover-delete-again");
        let (table, _) = three_parts(&db);
        let name = PartName::new(1, 1, 0);
        let doomed = table.dir().join("delete_tmp_all_1_1_0");
        fs::create_dir(&doomed).unwrap();
        fs::write(doomed.join("c0.bin"), b"partial").unwrap();
        assert!(delete_covered_part(table.dir(), name).unwrap());
        assert!(!doomed.exists());
        assert!(!table.dir().join(name.to_string()).exists());
        assert!(!delete_covered_part(table.dir(), name).unwrap());
    }

    /// A changed byte in each file type of §5.2 ends with part 2 in `detached/` and out of
    /// the next reader, through whichever check catches it.
    #[test]
    fn a_changed_byte_in_any_file_type_detaches_the_part() {
        for file in [
            "part.txt",
            "primary.idx",
            "c0.bin",
            "c0.mrk",
            "stats.bin",
            "sample.bin",
            "checksums.txt",
        ] {
            let db = TestDir::new("recover-each-file");
            let (table, _) = three_parts(&db);
            let broken = PartName::new(2, 2, 0);
            flip_byte(&table, broken, file);

            let snapshot = Snapshot::take(&table, None).unwrap();
            if file == "c0.bin" {
                // Only reading the block finds this one.
                assert!(snapshot.detached().is_empty(), "{file}");
                let err = snapshot.parts()[1].read_granule(0, 0).unwrap_err();
                assert_eq!(err.exit_code(), 3, "{file}");
                assert!(
                    err.to_string().starts_with("corrupt part all_2_2_0: "),
                    "{file}: {err}"
                );
                detach_corrupt(table.dir(), &err).unwrap();
            } else {
                assert_eq!(snapshot.detached().len(), 1, "{file}");
                assert_eq!(snapshot.detached()[0].name, broken, "{file}");
                assert!(
                    snapshot.detached()[0]
                        .message()
                        .starts_with("veta: detached broken part all_2_2_0: "),
                    "{file}"
                );
            }
            drop(snapshot);
            assert!(
                table
                    .dir()
                    .join(DETACHED_DIR)
                    .join("broken_all_2_2_0")
                    .is_dir(),
                "{file}"
            );
            let next = Snapshot::take(&table, None).unwrap();
            let names: Vec<PartName> = next.parts().iter().map(PartReader::name).collect();
            assert_eq!(
                names,
                vec![PartName::new(1, 1, 0), PartName::new(3, 3, 0)],
                "{file}"
            );
            assert!(next.detached().is_empty(), "{file}");
        }
    }

    #[test]
    fn only_corruption_detaches_and_only_a_part_name() {
        let db = TestDir::new("recover-detach-corrupt");
        let (table, _) = three_parts(&db);
        detach_corrupt(table.dir(), &VetaError::Io("disk".into())).unwrap();
        detach_corrupt(table.dir(), &VetaError::corrupt("roundtrip", "crc")).unwrap();
        assert_eq!(list_parts(table.dir()).unwrap(), sources_and_rest());
        assert!(!table.dir().join(DETACHED_DIR).exists());
        detach_corrupt(table.dir(), &VetaError::corrupt("all_1_1_0", "crc")).unwrap();
        assert!(!table.dir().join("all_1_1_0").exists());
        // A part another worker detached first is not an error.
        detach_corrupt(table.dir(), &VetaError::corrupt("all_1_1_0", "crc")).unwrap();
    }

    #[test]
    fn check_passes_on_a_healthy_table_and_touches_nothing() {
        let db = TestDir::new("check-healthy");
        let (table, before) = three_parts(&db);
        let report = check_table(&table).unwrap();
        assert_eq!(report.parts, 3);
        // Three columns, one granule per part.
        assert_eq!(report.blocks, 9);
        assert!(report.broken.is_empty() && report.corrupt.is_empty());
        assert_eq!(list_parts(table.dir()).unwrap(), sources_and_rest());
        assert_eq!(table_rows(&table), before);
    }

    #[test]
    fn check_detaches_what_fails_on_opening_and_on_reading() {
        let db = TestDir::new("check-corrupt");
        let (table, _) = three_parts(&db);
        flip_byte(&table, PartName::new(1, 1, 0), "sample.bin");
        flip_byte(&table, PartName::new(3, 3, 0), "c2.bin");

        let report = check_table(&table).unwrap();
        assert_eq!(report.parts, 3);
        assert_eq!(report.broken.len(), 1);
        assert_eq!(report.broken[0].name, PartName::new(1, 1, 0));
        assert_eq!(report.corrupt.len(), 1);
        assert_eq!(report.corrupt[0].name, PartName::new(3, 3, 0));
        assert_eq!(report.corrupt[0].error().exit_code(), 3);
        let detached = table.dir().join(DETACHED_DIR);
        assert!(detached.join("broken_all_1_1_0").is_dir());
        assert!(detached.join("broken_all_3_3_0").is_dir());
        assert_eq!(
            list_parts(table.dir()).unwrap(),
            vec![PartName::new(2, 2, 0)]
        );

        let again = check_table(&table).unwrap();
        assert_eq!(again.parts, 1);
        assert!(again.broken.is_empty() && again.corrupt.is_empty());
    }

    #[test]
    fn check_looks_at_covered_parts_too() {
        let db = TestDir::new("check-covered");
        let (table, _) = three_parts(&db);
        let merged = PartName::new(1, 2, 1);
        write_merged(&table, merged, 1, 2);
        flip_byte(&table, PartName::new(2, 2, 0), "c0.bin");
        let report = check_table(&table).unwrap();
        assert_eq!(report.parts, 4);
        assert_eq!(report.corrupt.len(), 1);
        assert_eq!(report.corrupt[0].name, PartName::new(2, 2, 0));
        assert!(table.dir().join(merged.to_string()).is_dir());
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
