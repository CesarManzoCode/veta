use std::ops::Bound;

use veta_core::rand::Rng;
use veta_core::{Chunk, DataType, Field, Schema, Value};

use super::*;
use crate::fsutil::testdir::TestDir;
use crate::insert::insert;
use crate::part::GRANULE_ROWS;
use crate::snapshot::Snapshot;
use crate::table::{Table, TableDef};

fn table(db: &TestDir, fields: Vec<Field>, primary_key: Vec<usize>) -> Table {
    let def = TableDef {
        name: "t".into(),
        schema: Schema::new(fields),
        primary_key,
        ddl: "CREATE TABLE t (unused INTEGER)".into(),
    };
    Table::create(db.path(), def).unwrap()
}

fn insert_rows(table: &Table, rows: &[Vec<Value>]) {
    let chunk = Chunk::from_rows(&table.schema().data_types(), rows).unwrap();
    insert(table, &chunk).unwrap();
}

fn int(v: i64) -> Value {
    Value::Int64(v)
}

fn range(column: usize, lower: Bound<Value>, upper: Bound<Value>) -> ScanFilter {
    ScanFilter::Range {
        column,
        lower,
        upper,
    }
}

/// Every row of every granule of `part`, with its granule.
fn rows_with_granules(part: &PartReader) -> Vec<(usize, Vec<Value>)> {
    let columns = part.schema().len();
    let mut out = Vec::new();
    for g in 0..part.granules() {
        let vectors: Vec<_> = (0..columns)
            .map(|c| part.read_granule(c, g).unwrap())
            .collect();
        for i in 0..part.granule_rows(g) {
            out.push((g, vectors.iter().map(|v| v.get(i)).collect()));
        }
    }
    out
}

fn row_matches(row: &[Value], filters: &[ScanFilter]) -> bool {
    filters.iter().all(|f| f.matches(&row[f.column()]))
}

/// The rows that match `filters` reading every granule (`--no-prune`) and reading only the
/// selected ones; also checks that the selection is an ordered subset of the granules.
fn compare(part: &PartReader, filters: &[ScanFilter]) -> (Vec<Vec<Value>>, Vec<Vec<Value>>) {
    let selected = prune_part(part, filters);
    assert!(selected.windows(2).all(|w| w[0] < w[1]));
    assert!(selected.iter().all(|&g| g < part.granules()));
    let mut all = Vec::new();
    let mut pruned = Vec::new();
    for (g, row) in rows_with_granules(part) {
        if row_matches(&row, filters) {
            if selected.contains(&g) {
                pruned.push(row.clone());
            }
            all.push(row);
        }
    }
    (all, pruned)
}

fn random_value(rng: &mut Rng, domain: i64) -> Value {
    int(rng.range_i64(-1, domain + 1))
}

fn random_bound(rng: &mut Rng, domain: i64) -> Bound<Value> {
    match rng.below(3) {
        0 => Bound::Unbounded,
        1 => Bound::Included(random_value(rng, domain)),
        _ => Bound::Excluded(random_value(rng, domain)),
    }
}

fn random_filter(rng: &mut Rng, column: usize, domain: i64) -> ScanFilter {
    match rng.below(3) {
        0 => ScanFilter::equal(column, random_value(rng, domain)),
        1 => ScanFilter::In {
            column,
            values: (0..1 + rng.below(3))
                .map(|_| random_value(rng, domain))
                .collect(),
        },
        _ => range(column, random_bound(rng, domain), random_bound(rng, domain)),
    }
}

#[test]
fn random_composite_keys_prune_without_losing_rows() {
    let db = TestDir::new("prune-random");
    // Key (a, b, c); b is nullable, v is outside the key.
    let t = table(
        &db,
        vec![
            Field::new("v", DataType::Int64, false),
            Field::new("a", DataType::Int64, false),
            Field::new("b", DataType::Int64, true),
            Field::new("c", DataType::Int64, false),
        ],
        vec![1, 2, 3],
    );
    let domains = [1000, 6, 40, 300];
    let mut rng = Rng::new(11);
    for batch in 0..3 {
        let rows: Vec<Vec<Value>> = (0..20_000 + batch * 3_333)
            .map(|i| {
                let b = if rng.next_bool(0.03) {
                    Value::Null
                } else {
                    int(rng.range_i64(0, domains[2]))
                };
                vec![
                    int(i as i64 % domains[0] + batch as i64 * 500),
                    int(rng.range_i64(0, domains[1])),
                    b,
                    int(rng.range_i64(0, domains[3])),
                ]
            })
            .collect();
        insert_rows(&t, &rows);
    }
    let snapshot = Snapshot::take(&t, None).unwrap();
    assert_eq!(snapshot.parts().len(), 3);
    let (mut total, mut read, mut matched) = (0usize, 0usize, 0usize);
    for _ in 0..300 {
        // Filters on a prefix of the key and, sometimes, on v.
        let prefix = 1 + rng.below(3) as usize;
        let mut filters = Vec::new();
        for c in 1..=prefix {
            if rng.next_bool(0.8) {
                filters.push(random_filter(&mut rng, c, domains[c]));
            }
        }
        if rng.next_bool(0.2) {
            filters.push(random_filter(&mut rng, 0, 2000));
        }
        for part in snapshot.parts() {
            let (all, pruned) = compare(part, &filters);
            assert_eq!(all, pruned, "filters {filters:?} in {}", part.name());
            total += part.granules();
            read += prune_part(part, &filters).len();
            matched += all.len();
        }
    }
    assert!(matched > 0);
    assert!(read < total, "{read} of {total} granules read");
}

/// a = 5 for rows 0..10000, 7 for 10000..20000 and 9 for 20000..26000: granules
/// 0 = [5, 5], 1 = [5, 7], 2 = [7, 9], 3 = [9, 9] (the last row of the part).
fn repeated_values_part(db: &TestDir) -> PartReader {
    let t = table(
        db,
        vec![
            Field::new("a", DataType::Int64, false),
            Field::new("b", DataType::Int64, false),
        ],
        vec![0, 1],
    );
    let rows: Vec<Vec<Value>> = (0..26_000i64)
        .map(|i| {
            let a = if i < 10_000 {
                5
            } else if i < 20_000 {
                7
            } else {
                9
            };
            vec![int(a), int(i)]
        })
        .collect();
    insert_rows(&t, &rows);
    Snapshot::take(&t, None).unwrap().into_parts().remove(0)
}

#[test]
fn repeated_values_cross_granules() {
    let db = TestDir::new("prune-repeated");
    let part = repeated_values_part(&db);
    assert_eq!(part.granules(), 4);
    assert_eq!(GRANULE_ROWS, 8192);
    let eq = |v| vec![ScanFilter::equal(0, int(v))];
    assert_eq!(prune_part(&part, &eq(5)), vec![0, 1]);
    assert_eq!(prune_part(&part, &eq(7)), vec![1, 2]);
    assert_eq!(prune_part(&part, &eq(6)), vec![1]);
    // 9 is the value of the last row of the part.
    assert_eq!(prune_part(&part, &eq(9)), vec![2, 3]);
    assert!(prune_part(&part, &eq(10)).is_empty());
    assert!(!part_may_match(&part, &eq(10)));
    assert!(!part_may_match(&part, &eq(4)));
    let gt9 = vec![range(0, Bound::Excluded(int(9)), Bound::Unbounded)];
    assert!(prune_part(&part, &gt9).is_empty());
    let ge9 = vec![range(0, Bound::Included(int(9)), Bound::Unbounded)];
    assert_eq!(prune_part(&part, &ge9), vec![2, 3]);
    let lt7 = vec![range(0, Bound::Unbounded, Bound::Excluded(int(7)))];
    assert_eq!(prune_part(&part, &lt7), vec![0, 1]);
    let set = vec![ScanFilter::In {
        column: 0,
        values: vec![int(5), int(9)],
    }];
    assert_eq!(prune_part(&part, &set), vec![0, 1, 2, 3]);
    for filters in [eq(5), eq(7), eq(9), gt9, ge9, lt7, set] {
        let (all, pruned) = compare(&part, &filters);
        assert_eq!(all, pruned);
    }
}

#[test]
fn second_key_column_is_bounded_only_under_an_equal_prefix() {
    let db = TestDir::new("prune-second");
    let part = repeated_values_part(&db);
    // Granule 0 is [(5, 0), (5, 8192)] and granule 3 [(9, 24576), (9, 25999)]: a is equal
    // at both ends, so b is bounded. In granules 1 and 2 a differs, so b is not.
    let b_small = vec![range(1, Bound::Unbounded, Bound::Excluded(int(100)))];
    assert_eq!(prune_part(&part, &b_small), vec![0, 1, 2]);
    let b_last = vec![ScanFilter::equal(1, int(25_999))];
    assert_eq!(prune_part(&part, &b_last), vec![1, 2, 3]);
    // b = 15000 lies in granule 1; granules 0 and 3 exclude it.
    let b_mid = vec![ScanFilter::equal(1, int(15_000))];
    assert_eq!(prune_part(&part, &b_mid), vec![1, 2]);
    let both = vec![
        ScanFilter::equal(0, int(7)),
        ScanFilter::equal(1, int(15_000)),
    ];
    assert_eq!(prune_part(&part, &both), vec![1, 2]);
    for filters in [b_small, b_last, b_mid, both] {
        let (all, pruned) = compare(&part, &filters);
        assert_eq!(all, pruned);
    }
}

#[test]
fn null_keys_and_column_stats() {
    let db = TestDir::new("prune-nulls");
    let t = table(
        &db,
        vec![
            Field::new("k", DataType::Int64, true),
            Field::new("v", DataType::Int64, true),
        ],
        vec![0],
    );
    // k: 0..10000 then 10000 NULLs, which sort last; v only NULL in the first part.
    let rows: Vec<Vec<Value>> = (0..20_000i64)
        .map(|i| {
            let k = if i < 10_000 { int(i) } else { Value::Null };
            vec![k, Value::Null]
        })
        .collect();
    insert_rows(&t, &rows);
    let rows: Vec<Vec<Value>> = (0..100i64).map(|i| vec![int(i), int(i + 50)]).collect();
    insert_rows(&t, &rows);
    let parts = Snapshot::take(&t, None).unwrap().into_parts();
    let (first, second) = (&parts[0], &parts[1]);
    assert_eq!(first.granules(), 3);
    // Granule 1 is [8192, NULL]: unbounded above, it may hold 9999.
    let ge = vec![range(0, Bound::Included(int(9_999)), Bound::Unbounded)];
    assert_eq!(prune_part(first, &ge), vec![1]);
    let lt = vec![range(0, Bound::Unbounded, Bound::Excluded(int(10)))];
    assert_eq!(prune_part(first, &lt), vec![0]);
    // Granule 2 holds only NULLs.
    let any = vec![range(0, Bound::Unbounded, Bound::Unbounded)];
    assert_eq!(prune_part(first, &any), vec![0, 1]);
    // v is NULL in every row of the first part; in the second it spans [50, 149].
    let v = vec![ScanFilter::equal(1, int(60))];
    assert!(prune_part(first, &v).is_empty());
    assert_eq!(prune_part(second, &v), vec![0]);
    let v_out = vec![range(1, Bound::Excluded(int(149)), Bound::Unbounded)];
    assert!(!part_may_match(second, &v_out));
    for filters in [ge, lt, any, v, v_out] {
        for part in &parts {
            let (all, pruned) = compare(part, &filters);
            assert_eq!(all, pruned);
        }
    }
}

#[test]
fn without_filters_or_key_every_granule_is_read() {
    let db = TestDir::new("prune-nokey");
    let t = table(&db, vec![Field::new("x", DataType::Int64, false)], vec![]);
    let rows: Vec<Vec<Value>> = (0..20_000i64).map(|i| vec![int(i)]).collect();
    insert_rows(&t, &rows);
    let part = Snapshot::take(&t, None).unwrap().into_parts().remove(0);
    assert_eq!(prune_part(&part, &[]), vec![0, 1, 2]);
    // Without a key only min/max can exclude the part.
    let inside = vec![ScanFilter::equal(0, int(5))];
    assert_eq!(prune_part(&part, &inside), vec![0, 1, 2]);
    let outside = vec![ScanFilter::equal(0, int(50_000))];
    assert!(prune_part(&part, &outside).is_empty());
}

/// One key column `a = row number` over 4 full granules: granule `g` holds
/// `[g·8192, (g+1)·8192)`.
fn sequential_part(db: &TestDir) -> PartReader {
    let t = table(db, vec![Field::new("a", DataType::Int64, false)], vec![0]);
    let rows: Vec<Vec<Value>> = (0..4 * GRANULE_ROWS as i64).map(|i| vec![int(i)]).collect();
    insert_rows(&t, &rows);
    Snapshot::take(&t, None).unwrap().into_parts().remove(0)
}

#[test]
fn last_granule_of_a_matching_range_is_kept() {
    let db = TestDir::new("prune-last-of-range");
    let part = sequential_part(&db);
    let g = GRANULE_ROWS as i64;
    // a in [g/2, 2g + g/2] spans granules 0, 1 and 2; granule 2 ends the run.
    let filters = vec![range(
        0,
        Bound::Included(int(g / 2)),
        Bound::Included(int(2 * g + g / 2)),
    )];
    assert_eq!(prune_part(&part, &filters), vec![0, 1, 2]);
    // A run of one granule: the last value of the part.
    let last = vec![ScanFilter::equal(0, int(4 * g - 1))];
    assert_eq!(prune_part(&part, &last), vec![3]);
    for filters in [filters, last] {
        let (all, pruned) = compare(&part, &filters);
        assert_eq!(all, pruned);
    }
}

#[test]
fn granule_range_is_bounded_by_the_next_index_entry() {
    let db = TestDir::new("prune-next-bound");
    let part = sequential_part(&db);
    let g = GRANULE_ROWS as i64;
    // Values strictly inside granule 1, never equal to an index entry.
    let inside = vec![ScanFilter::equal(0, int(g + 100))];
    assert_eq!(prune_part(&part, &inside), vec![1]);
    let tail = vec![range(
        0,
        Bound::Included(int(2 * g - 10)),
        Bound::Excluded(int(2 * g)),
    )];
    assert_eq!(prune_part(&part, &tail), vec![1]);
    for filters in [inside, tail] {
        let (all, pruned) = compare(&part, &filters);
        assert_eq!(all, pruned);
    }
}
