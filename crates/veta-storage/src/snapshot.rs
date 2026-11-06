//! Reader snapshot (contract §6.3).
//!
//! Under a shared `flock` of `.readers.lock` the part directories are listed, the active
//! set is computed (`tmp_*`, `delete_tmp_*` and `detached/` are not parts) and every active
//! part is opened and verified, keeping the `c<N>.bin` of the requested columns open; then
//! the lock is released. The open descriptors keep the data alive even if a merge cleanup
//! deletes the directories afterwards. If a part vanished while opening (`ENOENT`), the
//! snapshot starts over. The parts that fail their integrity checks are moved to
//! `detached/broken_<name>` and left out (§6.4); the snapshot then starts over too, since
//! the active set may have changed: what a broken part covered is active again. Readers
//! never delete anything.

use veta_core::{Result, VetaError};

use crate::locks::lock_shared;
use crate::part::{PartReader, active_parts, list_parts};
use crate::recover::{DetachedPart, detach_part};
use crate::table::{READERS_LOCK, Table};

/// Attempts before giving up on a table whose parts keep vanishing.
const MAX_ATTEMPTS: usize = 100;

/// The active parts of a table at one instant, opened and verified.
#[derive(Debug)]
pub struct Snapshot {
    parts: Vec<PartReader>,
    detached: Vec<DetachedPart>,
}

enum Attempt {
    Done(Vec<PartReader>),
    Vanished,
    Broken(Vec<DetachedPart>),
}

fn attempt(table: &Table, columns: &[usize]) -> Result<Attempt> {
    let _readers = lock_shared(&table.lock_path(READERS_LOCK))?;
    let active = active_parts(&list_parts(table.dir())?);
    let mut parts = Vec::with_capacity(active.len());
    let mut broken = Vec::new();
    for name in active {
        let dir = table.dir().join(name.to_string());
        match PartReader::open_columns(&dir, table.schema(), table.primary_key(), columns) {
            Ok(part) => parts.push(part),
            Err(_) if !dir.is_dir() => return Ok(Attempt::Vanished),
            Err(VetaError::Corrupt { reason, .. }) => broken.push(DetachedPart { name, reason }),
            Err(err) => return Err(err),
        }
    }
    if broken.is_empty() {
        Ok(Attempt::Done(parts))
    } else {
        Ok(Attempt::Broken(broken))
    }
}

impl Snapshot {
    /// Takes a snapshot of `table` keeping open the columns `columns` (every column if
    /// `None`).
    pub fn take(table: &Table, columns: Option<&[usize]>) -> Result<Snapshot> {
        let all: Vec<usize>;
        let columns = match columns {
            Some(columns) => columns,
            None => {
                all = (0..table.schema().len()).collect();
                &all
            }
        };
        let mut detached = Vec::new();
        for _ in 0..MAX_ATTEMPTS {
            match attempt(table, columns)? {
                Attempt::Done(parts) => return Ok(Snapshot { parts, detached }),
                Attempt::Vanished => {}
                Attempt::Broken(broken) => {
                    for part in broken {
                        detach_part(table.dir(), part.name)?;
                        detached.push(part);
                    }
                }
            }
        }
        Err(VetaError::Io(format!(
            "{}: parts kept vanishing while taking a snapshot",
            table.dir().display()
        )))
    }

    /// The active parts, in block order.
    pub fn parts(&self) -> &[PartReader] {
        &self.parts
    }

    pub fn into_parts(self) -> Vec<PartReader> {
        self.parts
    }

    /// Parts found broken and detached while taking the snapshot; the caller reports
    /// each with [`DetachedPart::message`] on stderr.
    pub fn detached(&self) -> &[DetachedPart] {
        &self.detached
    }

    /// Rows of the table: the exact sum over the parts (§11.5).
    pub fn rows(&self) -> u64 {
        self.parts.iter().map(PartReader::rows).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::testdir::TestDir;
    use crate::insert::insert;
    use crate::part::PartName;
    use crate::part::tests::write_part;
    use crate::sort::sort_chunk;
    use crate::table::DETACHED_DIR;
    use crate::table::tests::sample_def;
    use std::fs;
    use veta_core::{Chunk, Value};

    fn batch(table: &Table, n: i64, rows: i64) -> Chunk {
        let rows: Vec<Vec<Value>> = (0..rows)
            .map(|i| {
                vec![
                    Value::Int64(n * 100_000 + i),
                    Value::varchar(format!("s{}", i % 7)),
                    Value::Float64(i as f64),
                ]
            })
            .collect();
        Chunk::from_rows(&table.schema().data_types(), &rows).unwrap()
    }

    #[test]
    fn snapshot_sees_the_active_set_of_overlapping_parts() {
        let db = TestDir::new("snapshot-active");
        let table = Table::create(db.path(), sample_def("t")).unwrap();
        for n in 1..=3 {
            insert(&table, &batch(&table, n, 100 * n)).unwrap();
        }
        let mut merged = batch(&table, 1, 100);
        merged.extend(&batch(&table, 2, 200)).unwrap();
        let merged = sort_chunk(&merged, table.primary_key())
            .unwrap()
            .unwrap_or(merged);
        write_part(
            table.dir(),
            PartName::new(1, 2, 1),
            table.schema(),
            table.primary_key(),
            &merged,
            merged.len,
        );
        fs::create_dir(table.dir().join("tmp_insert_4")).unwrap();
        fs::create_dir(table.dir().join("delete_tmp_all_9_9_0")).unwrap();

        let snapshot = Snapshot::take(&table, None).unwrap();
        let names: Vec<PartName> = snapshot.parts().iter().map(PartReader::name).collect();
        assert_eq!(names, vec![PartName::new(1, 2, 1), PartName::new(3, 3, 0)]);
        assert_eq!(snapshot.rows(), 600);
        assert!(snapshot.detached().is_empty());
        // Covered parts are still on disk: readers never delete.
        assert!(table.dir().join("all_1_1_0").is_dir());
    }

    #[test]
    fn open_descriptors_survive_the_deletion_of_the_part() {
        let db = TestDir::new("snapshot-alive");
        let table = Table::create(db.path(), sample_def("t")).unwrap();
        let chunk = batch(&table, 1, 10_000);
        let name = insert(&table, &chunk).unwrap().unwrap();
        let snapshot = Snapshot::take(&table, Some(&[0][..])).unwrap();
        fs::remove_dir_all(table.dir().join(name.to_string())).unwrap();
        let part = &snapshot.parts()[0];
        assert!(part.has_column(0) && !part.has_column(1));
        let column = part.read_column(0).unwrap();
        assert_eq!(column.len(), 10_000);
        let mut values: Vec<Value> = (0..column.len()).map(|i| column.get(i)).collect();
        values.sort();
        let mut expected: Vec<Value> = (0..chunk.len).map(|i| chunk.column(0).get(i)).collect();
        expected.sort();
        assert_eq!(values, expected);
    }

    #[test]
    fn broken_parts_are_detached_and_left_out() {
        let db = TestDir::new("snapshot-broken");
        let table = Table::create(db.path(), sample_def("t")).unwrap();
        for n in 1..=2 {
            insert(&table, &batch(&table, n, 50)).unwrap();
        }
        let stats = table.dir().join("all_2_2_0").join("stats.bin");
        let mut bytes = fs::read(&stats).unwrap();
        bytes[10] ^= 0xff;
        fs::write(&stats, bytes).unwrap();

        let snapshot = Snapshot::take(&table, None).unwrap();
        let names: Vec<PartName> = snapshot.parts().iter().map(PartReader::name).collect();
        assert_eq!(names, vec![PartName::new(1, 1, 0)]);
        assert_eq!(snapshot.detached().len(), 1);
        assert_eq!(snapshot.detached()[0].name, PartName::new(2, 2, 0));
        assert!(
            table
                .dir()
                .join(DETACHED_DIR)
                .join("broken_all_2_2_0")
                .is_dir()
        );
        assert_eq!(snapshot.rows(), 50);
    }

    fn break_part(table: &Table, part: PartName) {
        let stats = table.dir().join(part.to_string()).join("stats.bin");
        let mut bytes = fs::read(&stats).unwrap();
        bytes[10] ^= 0xff;
        fs::write(&stats, bytes).unwrap();
    }

    #[test]
    fn every_broken_part_goes_in_one_pass_and_what_it_covered_comes_back() {
        let db = TestDir::new("snapshot-broken-many");
        let table = Table::create(db.path(), sample_def("t")).unwrap();
        for n in 1..=4 {
            insert(&table, &batch(&table, n, 50)).unwrap();
        }
        let mut merged = batch(&table, 1, 50);
        merged.extend(&batch(&table, 2, 50)).unwrap();
        let merged = sort_chunk(&merged, table.primary_key())
            .unwrap()
            .unwrap_or(merged);
        let covering = PartName::new(1, 2, 1);
        write_part(
            table.dir(),
            covering,
            table.schema(),
            table.primary_key(),
            &merged,
            merged.len,
        );
        break_part(&table, covering);
        break_part(&table, PartName::new(3, 3, 0));
        break_part(&table, PartName::new(1, 1, 0));

        let snapshot = Snapshot::take(&table, None).unwrap();
        let names: Vec<PartName> = snapshot.parts().iter().map(PartReader::name).collect();
        assert_eq!(names, vec![PartName::new(2, 2, 0), PartName::new(4, 4, 0)]);
        let mut detached: Vec<PartName> = snapshot.detached().iter().map(|d| d.name).collect();
        detached.sort();
        assert_eq!(
            detached,
            vec![PartName::new(1, 1, 0), covering, PartName::new(3, 3, 0)]
        );
    }
}
