//! Pruning end to end: the same rows with and without `--no-prune`, fewer granules and row
//! groups read with it (contract §11.3, §11.4, §13 `--stats`).

use std::path::PathBuf;
use std::sync::atomic::Ordering;

use veta_core::rand::Rng;
use veta_core::{Chunk, DataType, Value, Vector};
use veta_parquet::meta::{
    ColumnChunk, ColumnMetaData, CompressionCodec, DataPageHeader, Encoding, FieldRepetitionType,
    PageType, RowGroup, SchemaElement, Statistics, Type, encode,
};
use veta_parquet::{FileMetaData, PageHeader, write_footer};
use veta_sql::{OptimizerOptions, Statement, bind_query, optimize, parse_statement};
use veta_storage::insert;

use super::*;

struct TempDb {
    db: Database,
    dir: PathBuf,
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn temp_db(tag: &str) -> TempDb {
    let dir = std::env::temp_dir().join(format!("veta-prune-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    TempDb {
        db: Database::open(&dir),
        dir,
    }
}

/// Rows and counters of `sql` with the optimizer on, pruning on or off.
fn run(db: &Database, sql: &str, prune: bool) -> (Vec<Vec<Value>>, ExecStats) {
    let Statement::Query(q) = parse_statement(sql).unwrap() else {
        panic!("not a query: {sql}");
    };
    let plan = optimize(bind_query(db, &q).unwrap(), OptimizerOptions::default(), db);
    let stats = ExecStats::default();
    let result = execute(db, &plan, &ExecOptions { threads: 4, prune }, &stats)
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
    let rows = result
        .chunks
        .iter()
        .flat_map(|c| (0..c.len).map(move |i| c.row(i)))
        .collect();
    (rows, stats)
}

fn get(counter: &std::sync::atomic::AtomicU64) -> u64 {
    counter.load(Ordering::Relaxed)
}

fn keyed_table(tag: &str) -> TempDb {
    let t = temp_db(tag);
    let Statement::CreateTable(create) = parse_statement(
        "CREATE TABLE k (a INTEGER NOT NULL, b BIGINT NOT NULL, c VARCHAR NOT NULL, v BIGINT, \
         PRIMARY KEY (a, b, c))",
    )
    .unwrap() else {
        panic!("not a CREATE TABLE");
    };
    t.db.create_table(&create).unwrap();
    let table = t.db.open_table("k").unwrap();
    let mut rng = Rng::new(2024);
    let words = ["w", "x", "y", "z"];
    for batch in 0..3 {
        let n = 30_000 + batch * 1_234;
        let mut a = Vec::with_capacity(n);
        let mut b = Vec::with_capacity(n);
        let mut c = Vec::with_capacity(n);
        let mut v = Vec::with_capacity(n);
        for _ in 0..n {
            a.push(Value::Int32(rng.range_i64(0, 7) as i32));
            b.push(Value::Int64(rng.range_i64(0, 100)));
            c.push(Value::varchar(words[rng.below(4) as usize]));
            v.push(if rng.next_bool(0.1) {
                Value::Null
            } else {
                Value::Int64(rng.range_i64(-1000, 1000))
            });
        }
        let chunk = Chunk::new(vec![
            Vector::from_values(DataType::Int32, &a).unwrap(),
            Vector::from_values(DataType::Int64, &b).unwrap(),
            Vector::from_values(DataType::Varchar, &c).unwrap(),
            Vector::from_values(DataType::Int64, &v).unwrap(),
        ])
        .unwrap();
        insert(&table, &chunk).unwrap();
    }
    t
}

fn random_condition(rng: &mut Rng) -> String {
    let a = |rng: &mut Rng| rng.range_i64(-1, 9);
    let b = |rng: &mut Rng| rng.range_i64(-5, 105);
    let mut conjuncts = Vec::new();
    match rng.below(5) {
        0 => conjuncts.push(format!("a = {}", a(rng))),
        1 => {
            let lo = a(rng);
            conjuncts.push(format!("a BETWEEN {lo} AND {}", lo + rng.range_i64(0, 3)));
        }
        2 => conjuncts.push(format!("a IN ({}, {})", a(rng), a(rng))),
        3 => conjuncts.push(format!("a < {}", a(rng))),
        _ => conjuncts.push(format!("{} <= a", a(rng))),
    }
    if rng.next_bool(0.6) {
        conjuncts.push(match rng.below(3) {
            0 => format!("b = {}", b(rng)),
            1 => format!("b > {}", b(rng)),
            _ => format!("b IN ({}, {}, {})", b(rng), b(rng), b(rng)),
        });
        if rng.next_bool(0.5) {
            let w = ["w", "x", "y", "z", "q"][rng.below(5) as usize];
            conjuncts.push(format!("c = '{w}'"));
        }
    }
    if rng.next_bool(0.2) {
        conjuncts.push(format!("v > {}", rng.range_i64(-1200, 1200)));
    }
    conjuncts.join(" AND ")
}

#[test]
fn random_prefix_filters_read_fewer_granules_with_the_same_rows() {
    let t = keyed_table("random");
    let mut rng = Rng::new(9);
    let (mut fewer, mut matched) = (0, 0);
    for _ in 0..60 {
        let condition = random_condition(&mut rng);
        let sql = format!(
            "SELECT count(*), sum(v), min(b), max(c), count(DISTINCT a) FROM k WHERE {condition}"
        );
        let (pruned, with) = run(&t.db, &sql, true);
        let (full, without) = run(&t.db, &sql, false);
        assert_eq!(pruned, full, "{sql}");
        if pruned[0][0] != Value::Int64(0) {
            matched += 1;
        }
        assert_eq!(get(&with.parts_total), get(&without.parts_total));
        assert_eq!(get(&with.granules_total), get(&without.granules_total));
        assert_eq!(get(&without.granules_read), get(&without.granules_total));
        assert!(
            get(&with.granules_read) <= get(&without.granules_read),
            "{sql}"
        );
        assert!(get(&with.parts_read) <= get(&without.parts_read), "{sql}");
        assert!(get(&with.rows_read) <= get(&without.rows_read), "{sql}");
        if get(&with.granules_read) < get(&without.granules_read) {
            fewer += 1;
        }
        // The same rows, in key order, with a plain projection too.
        let rows_sql = format!("SELECT a, b, c, v FROM k WHERE {condition} ORDER BY a, b, c, v");
        assert_eq!(
            run(&t.db, &rows_sql, true).0,
            run(&t.db, &rows_sql, false).0,
            "{rows_sql}"
        );
    }
    assert!(matched > 0);
    assert!(fewer > 0);
}

#[test]
fn equality_on_the_first_key_column_reads_fewer_granules() {
    let t = keyed_table("equality");
    for sql in [
        "SELECT count(*) FROM k WHERE a = 3",
        // 7 is the largest a, the value of the last row of every part.
        "SELECT count(*) FROM k WHERE a = 7",
        "SELECT count(*) FROM k WHERE a = 0 AND b BETWEEN 10 AND 12",
    ] {
        let (pruned, with) = run(&t.db, sql, true);
        let (full, without) = run(&t.db, sql, false);
        assert_eq!(pruned, full, "{sql}");
        assert!(pruned[0][0] != Value::Int64(0), "{sql}");
        assert!(
            get(&with.granules_read) < get(&without.granules_read),
            "{sql}: {} vs {}",
            with.to_json(),
            without.to_json()
        );
        assert!(
            get(&with.rows_read) <= get(&with.granules_read) * 8192,
            "{sql}"
        );
        assert!(get(&with.rows_read) < get(&without.rows_read), "{sql}");
    }
    // Outside every key range: nothing is read.
    let (rows, stats) = run(&t.db, "SELECT count(*) FROM k WHERE a > 7", true);
    assert_eq!(rows, vec![vec![Value::Int64(0)]]);
    assert_eq!(get(&stats.parts_read), 0);
    assert_eq!(get(&stats.granules_read), 0);
    assert_eq!(get(&stats.rows_read), 0);
    assert_eq!(get(&stats.parts_total), 3);
}

#[test]
fn without_the_optimizer_nothing_is_pruned() {
    let t = keyed_table("no-optimizer");
    let Statement::Query(q) = parse_statement("SELECT count(*) FROM k WHERE a = 3").unwrap() else {
        panic!("not a query");
    };
    let plan = optimize(
        bind_query(&t.db, &q).unwrap(),
        OptimizerOptions {
            enabled: false,
            ..Default::default()
        },
        &t.db,
    );
    let stats = ExecStats::default();
    let on = execute(
        &t.db,
        &plan,
        &ExecOptions {
            threads: 2,
            prune: true,
        },
        &stats,
    )
    .unwrap();
    assert_eq!(get(&stats.granules_read), get(&stats.granules_total));
    let (rows, _) = run(&t.db, "SELECT count(*) FROM k WHERE a = 3", true);
    assert_eq!(on.chunks[0].row(0), rows[0]);
}

/// A Parquet file with one REQUIRED INT64 column `x` and one row group per slice of
/// `groups`, each with exact min/max statistics.
fn parquet_file(path: &std::path::Path, groups: &[Vec<i64>]) {
    let mut out = b"PAR1".to_vec();
    let mut row_groups = Vec::new();
    for values in groups {
        let start = out.len() as i64;
        let body: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let header = PageHeader {
            type_: PageType::DataPage,
            uncompressed_page_size: body.len() as i32,
            compressed_page_size: body.len() as i32,
            crc: None,
            data_page_header: Some(DataPageHeader {
                num_values: values.len() as i32,
                encoding: Encoding::Plain,
                definition_level_encoding: Encoding::Rle,
                repetition_level_encoding: Encoding::Rle,
                statistics: None,
            }),
            dictionary_page_header: None,
            data_page_header_v2: None,
        };
        let mut page = encode(&header);
        page.extend_from_slice(&body);
        out.extend_from_slice(&page);
        let min = values.iter().min().unwrap().to_le_bytes().to_vec();
        let max = values.iter().max().unwrap().to_le_bytes().to_vec();
        row_groups.push(RowGroup {
            columns: vec![ColumnChunk {
                file_path: None,
                file_offset: start,
                meta_data: Some(ColumnMetaData {
                    type_: Type::Int64,
                    encodings: vec![Encoding::Plain],
                    path_in_schema: vec!["x".into()],
                    codec: CompressionCodec::Uncompressed,
                    num_values: values.len() as i64,
                    total_uncompressed_size: page.len() as i64,
                    total_compressed_size: page.len() as i64,
                    key_value_metadata: None,
                    data_page_offset: start,
                    index_page_offset: None,
                    dictionary_page_offset: None,
                    statistics: Some(Statistics {
                        min_value: Some(min),
                        max_value: Some(max),
                        null_count: Some(0),
                        ..Statistics::default()
                    }),
                }),
            }],
            total_byte_size: page.len() as i64,
            num_rows: values.len() as i64,
            file_offset: Some(start),
            total_compressed_size: None,
            ordinal: None,
        });
    }
    let element = |name: &str, type_, repetition, children| SchemaElement {
        type_,
        type_length: None,
        repetition_type: repetition,
        name: name.into(),
        num_children: children,
        converted_type: None,
        scale: None,
        precision: None,
        field_id: None,
        logical_type: None,
    };
    let meta = FileMetaData {
        version: 1,
        schema: vec![
            element("schema", None, None, Some(1)),
            element(
                "x",
                Some(Type::Int64),
                Some(FieldRepetitionType::Required),
                None,
            ),
        ],
        num_rows: groups.iter().map(|g| g.len() as i64).sum(),
        row_groups,
        key_value_metadata: None,
        created_by: Some("veta test".into()),
        column_orders: None,
    };
    write_footer(&mut out, &meta).unwrap();
    std::fs::write(path, out).unwrap();
}

#[test]
fn parquet_row_groups_are_skipped_by_their_statistics() {
    let t = temp_db("parquet");
    let path = t.dir.join("x.parquet");
    let groups: Vec<Vec<i64>> = (0..4)
        .map(|g| (0..1000).map(|i| g * 1000 + i).collect())
        .collect();
    parquet_file(&path, &groups);
    let file = path.display();
    for (condition, groups_read) in [
        ("x BETWEEN 1500 AND 2500", 2),
        ("x = 3999", 1),
        ("x IN (5, 2000)", 2),
        ("x > 5000", 0),
        ("x >= 0", 4),
    ] {
        let sql = format!("SELECT count(*), sum(x) FROM read_parquet('{file}') WHERE {condition}");
        let (pruned, with) = run(&t.db, &sql, true);
        let (full, without) = run(&t.db, &sql, false);
        assert_eq!(pruned, full, "{sql}");
        assert_eq!(get(&with.row_groups_total), 4);
        assert_eq!(get(&without.row_groups_read), 4);
        assert_eq!(get(&with.row_groups_read), groups_read, "{sql}");
        assert_eq!(get(&with.rows_read), groups_read * 1000, "{sql}");
    }
}
