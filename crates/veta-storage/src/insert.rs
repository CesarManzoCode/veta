//! Insertion: one batch = one part = atomic (contract §6.1).
//!
//! 1. Exclusive `flock` of `.insert.lock`; the block number is one more than the larger of
//!    `block_number.txt` and the largest `max_block` present; `block_number.txt` is
//!    rewritten (tmp, fsync, rename).
//! 2. `tmp_insert_<b>/` is created and its `owner.lock` locked exclusively until the end;
//!    `.insert.lock` is released.
//! 3. The part is written with every file and the directory fsynced.
//! 4. `rename(tmp_insert_<b>, all_<b>_<b>_0)` and fsync of the table directory.
//!
//! The batch is sorted by the primary key first ([`crate::sort`]). An empty batch creates
//! no part and takes no block number. After the rename the published part still holds
//! `owner.lock`; it is removed right away (its descriptor stays locked until the end), and
//! recovery removes it should the process die in between, because removing it before the
//! rename would let a concurrent recovery take a fresh `owner.lock` of a live `tmp_*`.
//!
//! §6.1 releases `.insert.lock` before step 2. Here it is released after it, so a block
//! number is never assigned without its `tmp_insert_<b>` being visible: a merge choosing
//! its window under that lock ([`crate::merge`]) must not span a block still being
//! written, or the part published later would be covered, and so lost, at birth. Recovery
//! relies on the same lock ([`create_owned_tmp`]).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use veta_core::{Chunk, Result, Schema, VetaError};

use crate::fsutil::{io_err, remove_dir_all_if_exists, sync_dir, write_atomic};
use crate::locks::{FileLock, lock_exclusive};
use crate::part::{OWNER_LOCK, PartName, PartWriter, list_parts};
use crate::sort::sort_chunk;
use crate::table::{BLOCK_NUMBER_TXT, INSERT_LOCK, TMP_INSERT_PREFIX, Table};

/// Checks that `chunk` fits `schema`: its column count and types (an engine bug otherwise)
/// and no NULL in a NOT NULL column (a user error).
pub fn check_batch(schema: &Schema, chunk: &Chunk) -> Result<()> {
    if chunk.num_columns() != schema.len() {
        return Err(VetaError::internal(format!(
            "batch has {} columns, the table {}",
            chunk.num_columns(),
            schema.len()
        )));
    }
    for (vector, field) in chunk.columns.iter().zip(&schema.fields) {
        if vector.data_type() != field.data_type {
            return Err(VetaError::internal(format!(
                "column {} is {}, the batch has {}",
                field.name,
                field.data_type,
                vector.data_type()
            )));
        }
        if !field.nullable && vector.null_count() > 0 {
            return Err(VetaError::user(format!(
                "NULL value in NOT NULL column {}",
                field.name
            )));
        }
    }
    Ok(())
}

/// Reads the watermark of `block_number.txt` (0 if the file is missing).
fn read_block_number(path: &Path) -> Result<u64> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(io_err(path, e)),
    };
    let trimmed = text.trim();
    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return Err(VetaError::Io(format!(
            "{}: invalid block number {trimmed:?}",
            path.display()
        )));
    }
    trimmed
        .parse()
        .map_err(|_| VetaError::Io(format!("{}: block number out of range", path.display())))
}

/// Assigns the next block number; the caller holds `.insert.lock`.
fn next_block_number(table: &Table) -> Result<u64> {
    let path = table.dir().join(BLOCK_NUMBER_TXT);
    let watermark = read_block_number(&path)?;
    let present = list_parts(table.dir())?
        .iter()
        .map(|p| p.max_block)
        .max()
        .unwrap_or(0);
    let block = watermark
        .max(present)
        .checked_add(1)
        .ok_or_else(|| VetaError::internal("block numbers exhausted"))?;
    write_atomic(&path, format!("{block}\n").as_bytes())?;
    Ok(block)
}

/// Creates the part directory under construction `tmp` and takes its `owner.lock` before
/// anything is written in it. The caller holds `.insert.lock`, shared or exclusive: recovery
/// looks at the `tmp_*` under the exclusive lock, so it never finds a live one between the
/// `mkdir` and the `flock`, where `try_lock` would succeed and pass its owner for dead.
pub(crate) fn create_owned_tmp(tmp: &Path) -> Result<FileLock> {
    fs::create_dir(tmp).map_err(|e| io_err(tmp, e))?;
    match lock_exclusive(&tmp.join(OWNER_LOCK)) {
        Ok(owner) => Ok(owner),
        Err(err) => {
            let _ = remove_dir_all_if_exists(tmp);
            Err(err)
        }
    }
}

/// Steps 1 and 2 of §6.1: the block number, its temporary directory and its held
/// `owner.lock`, all under `.insert.lock`.
fn begin_insert(table: &Table) -> Result<(u64, PathBuf, FileLock)> {
    let _lock = lock_exclusive(&table.lock_path(INSERT_LOCK))?;
    let block = next_block_number(table)?;
    let tmp = table.dir().join(format!("{TMP_INSERT_PREFIX}{block}"));
    let owner = create_owned_tmp(&tmp)?;
    Ok((block, tmp, owner))
}

/// Inserts `chunk` as one new part; returns its name, or `None` for an empty batch.
pub fn insert(table: &Table, chunk: &Chunk) -> Result<Option<PartName>> {
    check_batch(table.schema(), chunk)?;
    if chunk.len == 0 {
        return Ok(None);
    }
    let sorted = sort_chunk(chunk, table.primary_key())?;
    let rows = sorted.as_ref().unwrap_or(chunk);

    let (block, tmp, owner) = begin_insert(table)?;
    let name = PartName::new(block, block, 0);
    let result = write_and_publish(table, &tmp, name, rows);
    if result.is_err() {
        let _ = remove_dir_all_if_exists(&tmp);
    }
    drop(owner);
    result.map(|()| Some(name))
}

/// Steps 3 and 4 of §6.1.
fn write_and_publish(table: &Table, tmp: &Path, name: PartName, rows: &Chunk) -> Result<()> {
    let mut writer = PartWriter::create(
        tmp,
        name,
        table.schema(),
        table.primary_key(),
        rows.len as u64,
    )?;
    writer.write(rows)?;
    writer.finish()?;
    publish_part(table, tmp, name)
}

/// Renames the finished `tmp` to its part name, fsyncs the table directory and removes the
/// `owner.lock` it carried along (step 4 of §6.1, step 3 of §6.2).
pub(crate) fn publish_part(table: &Table, tmp: &Path, name: PartName) -> Result<()> {
    let dest = table.dir().join(name.to_string());
    fs::rename(tmp, &dest).map_err(|e| io_err(&dest, e))?;
    sync_dir(table.dir())?;
    match fs::remove_file(dest.join(OWNER_LOCK)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err(&dest.join(OWNER_LOCK), e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::subdirectories;
    use crate::fsutil::testdir::TestDir;
    use crate::locks::try_lock_exclusive;
    use crate::part::PartReader;
    use crate::part::tests::write_part;
    use crate::table::tests::sample_def;
    use std::cmp::Ordering;
    use veta_core::rand::Rng;
    use veta_core::{DataType, Value};

    fn rows_of(schema: &Schema, rows: &[Vec<Value>]) -> Chunk {
        Chunk::from_rows(&schema.data_types(), rows).unwrap()
    }

    #[test]
    fn an_empty_batch_creates_no_part() {
        let db = TestDir::new("insert-empty");
        let table = Table::create(db.path(), sample_def("t")).unwrap();
        let empty = Chunk::empty(&table.schema().data_types());
        assert_eq!(insert(&table, &empty).unwrap(), None);
        assert!(list_parts(table.dir()).unwrap().is_empty());
        assert!(subdirectories(table.dir()).unwrap().is_empty());
        assert_eq!(
            fs::read_to_string(table.dir().join(BLOCK_NUMBER_TXT)).unwrap(),
            "0\n"
        );
    }

    #[test]
    fn consecutive_inserts_get_consecutive_blocks() {
        let db = TestDir::new("insert-blocks");
        let table = Table::create(db.path(), sample_def("t")).unwrap();
        let schema = table.schema().clone();
        let batch = rows_of(
            &schema,
            &[vec![Value::Int64(1), Value::varchar("x"), Value::Null]],
        );
        let first = insert(&table, &batch).unwrap().unwrap();
        let second = insert(&table, &batch).unwrap().unwrap();
        assert_eq!(first, PartName::new(1, 1, 0));
        assert_eq!(second, PartName::new(2, 2, 0));
        assert_eq!(
            subdirectories(table.dir()).unwrap(),
            vec!["all_1_1_0", "all_2_2_0"]
        );
        assert_eq!(
            fs::read_to_string(table.dir().join(BLOCK_NUMBER_TXT)).unwrap(),
            "2\n"
        );
        for part in [first, second] {
            let dir = table.dir().join(part.to_string());
            assert!(!dir.join(OWNER_LOCK).exists());
            let reader = PartReader::open(&dir, &schema, table.primary_key()).unwrap();
            assert_eq!(reader.rows(), 1);
        }
    }

    #[test]
    fn a_block_number_comes_with_its_visible_held_tmp_dir() {
        let db = TestDir::new("insert-begin");
        let table = Table::create(db.path(), sample_def("t")).unwrap();
        let (block, tmp, owner) = begin_insert(&table).unwrap();
        assert_eq!(block, 1);
        assert_eq!(tmp, table.dir().join("tmp_insert_1"));
        assert!(try_lock_exclusive(&tmp.join(OWNER_LOCK)).unwrap().is_none());
        // `.insert.lock` is free again, so the next insert can start.
        let (block, _, _second) = begin_insert(&table).unwrap();
        assert_eq!(block, 2);
        drop(owner);
    }

    #[test]
    fn block_numbers_skip_past_parts_already_present() {
        let db = TestDir::new("insert-present");
        let table = Table::create(db.path(), sample_def("t")).unwrap();
        let schema = table.schema().clone();
        let batch = rows_of(
            &schema,
            &[vec![Value::Int64(1), Value::varchar("x"), Value::Null]],
        );
        write_part(
            table.dir(),
            PartName::new(3, 7, 1),
            &schema,
            table.primary_key(),
            &batch,
            1,
        );
        assert_eq!(
            insert(&table, &batch).unwrap(),
            Some(PartName::new(8, 8, 0))
        );
    }

    #[test]
    fn null_in_not_null_column_is_a_user_error() {
        let db = TestDir::new("insert-null");
        let table = Table::create(db.path(), sample_def("t")).unwrap();
        let batch = rows_of(
            table.schema(),
            &[vec![Value::Null, Value::varchar("x"), Value::Null]],
        );
        let err = insert(&table, &batch).unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(list_parts(table.dir()).unwrap().is_empty());
        let wrong = Chunk::empty(&[DataType::Int64]);
        assert!(insert(&table, &wrong).is_err());
    }

    #[test]
    fn rows_are_ordered_by_the_composite_key_with_repeats() {
        let db = TestDir::new("insert-order");
        let table = Table::create(db.path(), sample_def("t")).unwrap();
        let schema = table.schema().clone();
        let mut rng = Rng::new(77);
        let rows: Vec<Vec<Value>> = (0..20_000)
            .map(|i| {
                let b = if rng.next_bool(0.05) {
                    Value::Null
                } else {
                    Value::varchar(["k", "a", "kk", ""][rng.below(4) as usize])
                };
                vec![
                    Value::Int64(rng.range_i64(-5, 5)),
                    b,
                    Value::Float64(i as f64),
                ]
            })
            .collect();
        let part = insert(&table, &rows_of(&schema, &rows)).unwrap().unwrap();
        let reader = PartReader::open(
            &table.dir().join(part.to_string()),
            &schema,
            table.primary_key(),
        )
        .unwrap();
        let columns: Vec<_> = (0..3).map(|c| reader.read_column(c).unwrap()).collect();
        let mut expected = rows.clone();
        // Key (b, a); the stable sort keeps insertion order (column c) among equal keys.
        expected.sort_by(|x, y| x[1].cmp(&y[1]).then_with(|| x[0].cmp(&y[0])));
        for (i, row) in expected.iter().enumerate() {
            for c in 0..3 {
                assert_eq!(&columns[c].get(i), &row[c], "row {i} column {c}");
            }
        }
        for i in 1..expected.len() {
            let prev = (columns[1].get(i - 1), columns[0].get(i - 1));
            let next = (columns[1].get(i), columns[0].get(i));
            assert_ne!(prev.cmp(&next), Ordering::Greater, "row {i}");
        }
    }
}
