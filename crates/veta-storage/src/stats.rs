//! Table statistics for cardinality estimation (contract §5.2, §11.5): the `stats.bin` and
//! `sample.bin` of every active part combined into one [`TableStats`].
//!
//! Rows and NULLs are summed, min/max widened, column and key-prefix HLLs merged register
//! by register and the part samples concatenated in block order. Reading statistics never
//! detaches anything: a part that fails its checks is left out here and is detached (and
//! reported) by the [`Snapshot`](crate::Snapshot) the query takes to read its rows.

use veta_core::stats::TableStats;
use veta_core::{Result, VetaError};

use crate::locks::lock_shared;
use crate::part::{PartReader, active_parts, list_parts};
use crate::table::{READERS_LOCK, Table};

/// Statistics of one opened part as a [`TableStats`].
pub fn part_table_stats(part: &PartReader) -> TableStats {
    let stats = part.stats();
    TableStats {
        rows: part.rows(),
        columns: stats.columns.clone(),
        key_prefix_hll: stats.key_prefix_hll.clone(),
        sample: part.sample().clone(),
    }
}

/// Union of the statistics of `parts` (all of `table`'s shape).
pub fn merge_part_stats(table: &Table, parts: &[PartReader]) -> Result<TableStats> {
    let mut out = TableStats::new(&table.schema().data_types(), table.primary_key().len());
    for part in parts {
        out.merge(&part_table_stats(part))?;
    }
    Ok(out)
}

/// Statistics of the active parts of `table` at one instant (§11.5). Opens each part
/// without keeping any `c<N>.bin`; a part that vanished or is corrupt is skipped.
pub fn table_stats(table: &Table) -> Result<TableStats> {
    let parts = {
        let _readers = lock_shared(&table.lock_path(READERS_LOCK))?;
        let mut parts = Vec::new();
        for name in active_parts(&list_parts(table.dir())?) {
            let dir = table.dir().join(name.to_string());
            match PartReader::open_columns(&dir, table.schema(), table.primary_key(), &[]) {
                Ok(part) => parts.push(part),
                Err(VetaError::Corrupt { .. }) => {}
                Err(_) if !dir.is_dir() => {}
                Err(err) => return Err(err),
            }
        }
        parts
    };
    merge_part_stats(table, &parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::testdir::TestDir;
    use crate::insert::insert;
    use crate::table::tests::sample_def;
    use std::fs;
    use veta_core::stats::SAMPLE_ROWS;
    use veta_core::{Chunk, Value};

    /// Rows `start..start + n` with `a = i`, `b = "s{i % 50}"` and `c` NULL on every
    /// fourth row.
    fn batch(table: &Table, start: i64, n: i64) -> Chunk {
        let rows: Vec<Vec<Value>> = (start..start + n)
            .map(|i| {
                vec![
                    Value::Int64(i),
                    Value::varchar(format!("s{}", i % 50)),
                    if i % 4 == 0 {
                        Value::Null
                    } else {
                        Value::Float64(i as f64)
                    },
                ]
            })
            .collect();
        Chunk::from_rows(&table.schema().data_types(), &rows).unwrap()
    }

    fn close(estimate: f64, truth: f64) -> bool {
        (estimate - truth).abs() <= 0.05 * truth
    }

    #[test]
    fn parts_merge_into_table_stats() {
        let db = TestDir::new("stats-merge");
        let table = Table::create(db.path(), sample_def("t")).unwrap();
        // Two overlapping parts: a in 0..3000 and 2000..6000.
        insert(&table, &batch(&table, 0, 3000)).unwrap();
        insert(&table, &batch(&table, 2000, 4000)).unwrap();
        let stats = table_stats(&table).unwrap();

        assert_eq!(stats.rows, 7000);
        assert_eq!(stats.columns[0].rows, 7000);
        assert_eq!(stats.columns[0].nulls, 0);
        assert_eq!(stats.columns[0].min, Some(Value::Int64(0)));
        assert_eq!(stats.columns[0].max, Some(Value::Int64(5999)));
        assert_eq!(stats.columns[2].nulls, 750 + 1000);
        // The merged HLL counts the union, not the sum.
        assert!(
            close(stats.columns[0].ndv(), 6000.0),
            "{}",
            stats.columns[0].ndv()
        );
        assert!(close(stats.columns[1].ndv(), 50.0));
        // Key (b, a): prefix 1 is b, prefix 2 is the (b, a) pair, unique per a.
        assert_eq!(stats.key_prefix_hll.len(), 2);
        assert!(close(stats.key_prefix_hll[0].estimate(), 50.0));
        assert!(close(stats.key_prefix_hll[1].estimate(), 6000.0));
        // The sample is the union of both part samples.
        assert_eq!(stats.sample.len, 2 * SAMPLE_ROWS as usize);
        assert_eq!(stats.sample.num_columns(), 3);
    }

    #[test]
    fn key_prefix_hlls_union_across_parts() {
        let db = TestDir::new("stats-prefix");
        let table = Table::create(db.path(), sample_def("t")).unwrap();
        insert(&table, &batch(&table, 0, 500)).unwrap();
        insert(&table, &batch(&table, 0, 500)).unwrap();
        let stats = table_stats(&table).unwrap();
        // The same keys twice: the HLLs equal those of one part.
        let single = {
            let db = TestDir::new("stats-prefix-one");
            let one = Table::create(db.path(), sample_def("t")).unwrap();
            insert(&one, &batch(&one, 0, 500)).unwrap();
            table_stats(&one).unwrap()
        };
        assert_eq!(stats.rows, 1000);
        assert_eq!(stats.key_prefix_hll, single.key_prefix_hll);
        assert_eq!(stats.columns[0].hll, single.columns[0].hll);
        assert_eq!(stats.sample.len, 1000);
    }

    #[test]
    fn empty_tables_and_broken_parts() {
        let db = TestDir::new("stats-broken");
        let table = Table::create(db.path(), sample_def("t")).unwrap();
        let empty = table_stats(&table).unwrap();
        assert_eq!(empty.rows, 0);
        assert_eq!(empty.sample.len, 0);

        insert(&table, &batch(&table, 0, 100)).unwrap();
        insert(&table, &batch(&table, 100, 100)).unwrap();
        let path = table.dir().join("all_2_2_0").join("stats.bin");
        let mut bytes = fs::read(&path).unwrap();
        bytes[10] ^= 0xff;
        fs::write(&path, bytes).unwrap();
        let stats = table_stats(&table).unwrap();
        assert_eq!(stats.rows, 100);
        // Statistics never detach: the broken part is still in place.
        assert!(table.dir().join("all_2_2_0").is_dir());
    }
}
