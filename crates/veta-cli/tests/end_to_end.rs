//! End-to-end tests of the `veta` binary: a database in `CARGO_TARGET_TMPDIR` loaded from
//! generated TSV in several batches, with each result compared against the same result
//! computed by hand in Rust.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use veta_core::datetime::format_date;
use veta_core::decimal;
use veta_core::format::write_escaped;
use veta_core::rand::Rng;
use veta_parquet::footer::{MAGIC, write_footer};
use veta_parquet::meta::{
    ColumnChunk, ColumnMetaData, CompressionCodec, DataPageHeader, Encoding, FieldRepetitionType,
    FileMetaData, PageHeader, PageType, RowGroup, SchemaElement, Type, encode,
};

const CREATE: &str = "CREATE TABLE t (id BIGINT NOT NULL, grp BIGINT NOT NULL, label VARCHAR, \
                      val INTEGER, price DECIMAL(10,2) NOT NULL, day DATE NOT NULL, \
                      PRIMARY KEY (grp, id)) ENGINE = MergeTree";
const LABELS: [&str; 6] = ["alpha", "beta", "gamma", "alpine", "", "tab\tbed"];

#[derive(Debug, Clone)]
struct Row {
    id: i64,
    grp: i64,
    label: Option<String>,
    val: Option<i32>,
    price: i64,
    day: i32,
}

fn veta(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_veta"))
        .args(args)
        .output()
        .expect("the binary runs")
}

/// An empty directory for one test.
fn test_dir(tag: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn text(path: &Path) -> &str {
    path.to_str().expect("UTF-8 path")
}

fn escaped(s: &str) -> String {
    let mut out = Vec::new();
    write_escaped(&mut out, s.as_bytes());
    String::from_utf8(out).unwrap()
}

fn generate_rows(seed: u64, n: i64) -> Vec<Row> {
    let mut rng = Rng::new(seed);
    (0..n)
        .map(|id| Row {
            id,
            grp: rng.range_i64(0, 2999),
            label: rng
                .next_bool(0.8)
                .then(|| LABELS[rng.below(LABELS.len() as u64) as usize].to_string()),
            val: rng
                .next_bool(0.75)
                .then(|| rng.range_i64(-1000, 1000) as i32),
            price: rng.range_i64(-50_000, 50_000),
            day: rng.range_i64(15_000, 15_400) as i32,
        })
        .collect()
}

fn tsv_line(row: &Row) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{}\t{}\n",
        row.id,
        row.grp,
        row.label.as_deref().map_or("\\N".to_string(), escaped),
        row.val.map_or("\\N".to_string(), |v| v.to_string()),
        decimal::format(row.price as i128, 2),
        format_date(row.day),
    )
}

/// A database with table `t`, loaded with `rows` in three files, the last split in batches.
fn loaded_database(tag: &str, rows: &[Row]) -> PathBuf {
    let dir = test_dir(tag);
    let db = dir.join("db");
    let done = veta(&["sql", "--db", text(&db), CREATE]);
    assert_eq!(done.status.code(), Some(0), "{done:?}");
    let third = rows.len() / 3;
    for (i, batch) in [&rows[..third], &rows[third..2 * third], &rows[2 * third..]]
        .iter()
        .enumerate()
    {
        let path = dir.join(format!("batch{i}.tsv"));
        let content: String = batch.iter().map(tsv_line).collect();
        fs::write(&path, content).unwrap();
        let mut args = vec![
            "load",
            "--db",
            text(&db),
            "--table",
            "t",
            "--tsv",
            text(&path),
        ];
        if i == 2 {
            args.extend(["--batch-rows", "4000"]);
        }
        let done = veta(&args);
        assert_eq!(done.status.code(), Some(0), "{done:?}");
    }
    db
}

/// Output of a query, which must succeed.
fn sql(db: &Path, threads: &str, query: &str) -> Vec<String> {
    let done = veta(&["sql", "--db", text(db), "--threads", threads, query]);
    assert_eq!(done.status.code(), Some(0), "{query}: {done:?}");
    String::from_utf8(done.stdout)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

fn sorted(mut lines: Vec<String>) -> Vec<String> {
    lines.sort();
    lines
}

fn null_or<T: ToString>(value: Option<T>) -> String {
    value.map_or("\\N".to_string(), |v| v.to_string())
}

#[test]
fn single_table_queries_match_hand_computed_results() {
    let rows = generate_rows(42, 30_000);
    let db = loaded_database("hand", &rows);

    let n = rows.len();
    let vals: Vec<i64> = rows.iter().filter_map(|r| r.val.map(i64::from)).collect();
    let labels: BTreeSet<&str> = rows.iter().filter_map(|r| r.label.as_deref()).collect();
    let groups: BTreeSet<i64> = rows.iter().map(|r| r.grp).collect();
    let price: i64 = rows.iter().map(|r| r.price).sum();

    for threads in ["1", "8"] {
        assert_eq!(
            sql(
                &db,
                threads,
                "SELECT count(*), count(val), count(label) FROM t"
            ),
            vec![format!(
                "{n}\t{}\t{}",
                vals.len(),
                rows.iter().filter(|r| r.label.is_some()).count()
            )]
        );
        assert_eq!(
            sql(&db, threads, "SELECT sum(val), min(val), max(val) FROM t"),
            vec![format!(
                "{}\t{}\t{}",
                vals.iter().sum::<i64>(),
                vals.iter().min().unwrap(),
                vals.iter().max().unwrap()
            )]
        );
        let avg = sql(&db, threads, "SELECT avg(val) FROM t");
        let got: f64 = avg[0].parse().unwrap();
        let want = vals.iter().sum::<i64>() as f64 / vals.len() as f64;
        assert!(
            (got - want).abs() <= 1e-9 * want.abs().max(1.0),
            "{got} vs {want}"
        );
        assert_eq!(
            sql(&db, threads, "SELECT sum(price), min(day), max(day) FROM t"),
            vec![format!(
                "{}\t{}\t{}",
                decimal::format(price as i128, 2),
                format_date(rows.iter().map(|r| r.day).min().unwrap()),
                format_date(rows.iter().map(|r| r.day).max().unwrap()),
            )]
        );
        assert_eq!(
            sql(
                &db,
                threads,
                "SELECT count(DISTINCT label), count(DISTINCT grp), count(DISTINCT val) FROM t"
            ),
            vec![format!(
                "{}\t{}\t{}",
                labels.len(),
                groups.len(),
                vals.iter().collect::<BTreeSet<_>>().len()
            )]
        );
    }
}

#[test]
fn group_by_with_many_keys_matches_for_any_thread_count() {
    let rows = generate_rows(7, 30_000);
    let db = loaded_database("group", &rows);

    let mut by_grp: BTreeMap<i64, (i64, Option<i64>, BTreeSet<Option<&str>>)> = BTreeMap::new();
    for row in &rows {
        let entry = by_grp.entry(row.grp).or_insert((0, None, BTreeSet::new()));
        entry.0 += 1;
        if let Some(v) = row.val {
            entry.1 = Some(entry.1.unwrap_or(0) + i64::from(v));
        }
        if row.label.is_some() {
            entry.2.insert(row.label.as_deref());
        }
    }
    let want: Vec<String> = sorted(
        by_grp
            .iter()
            .map(|(grp, (count, sum, labels))| {
                format!("{grp}\t{count}\t{}\t{}", null_or(*sum), labels.len())
            })
            .collect(),
    );
    let by_grp_sql = "SELECT grp, count(*), sum(val), count(DISTINCT label) FROM t GROUP BY grp";

    let mut by_label: BTreeMap<Option<&str>, (i64, Option<i32>)> = BTreeMap::new();
    for row in &rows {
        let entry = by_label.entry(row.label.as_deref()).or_insert((0, None));
        entry.0 += 1;
        entry.1 = match (entry.1, row.val) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
    }
    let want_label: Vec<String> = sorted(
        by_label
            .iter()
            .map(|(label, (count, min))| {
                let name = label.map_or("\\N".to_string(), escaped);
                format!("{name}\t{count}\t{}", null_or(*min))
            })
            .collect(),
    );
    let by_label_sql = "SELECT label, count(*), min(val) FROM t GROUP BY label";

    let mut by_both: BTreeMap<(i64, Option<&str>), i64> = BTreeMap::new();
    for row in &rows {
        *by_both
            .entry((row.grp % 50, row.label.as_deref()))
            .or_insert(0) += 1;
    }
    let want_both: Vec<String> = sorted(
        by_both
            .iter()
            .map(|((grp, label), count)| {
                let name = label.map_or("\\N".to_string(), escaped);
                format!("{grp}\t{name}\t{count}")
            })
            .collect(),
    );
    let by_both_sql = "SELECT grp % 50, label, count(*) FROM t GROUP BY grp % 50, label";

    let days: BTreeSet<i32> = rows.iter().map(|r| r.day).collect();
    let want_distinct: Vec<String> = days.iter().map(|&d| format_date(d)).collect();

    for threads in ["1", "8"] {
        assert_eq!(sorted(sql(&db, threads, by_grp_sql)), want);
        assert_eq!(sorted(sql(&db, threads, by_label_sql)), want_label);
        assert_eq!(sorted(sql(&db, threads, by_both_sql)), want_both);
        assert_eq!(
            sql(&db, threads, "SELECT DISTINCT day FROM t ORDER BY day"),
            want_distinct
        );
    }
}

#[test]
fn order_by_with_limit_and_offset_is_exact() {
    let rows = generate_rows(9, 20_000);
    let db = loaded_database("order", &rows);

    let mut ordered = rows.clone();
    ordered.sort_by(|a, b| b.grp.cmp(&a.grp).then(a.id.cmp(&b.id)));
    let want: Vec<String> = ordered[10..35]
        .iter()
        .map(|r| format!("{}\t{}", r.grp, r.id))
        .collect();

    // NULL goes last in ascending order and first in descending order.
    let mut ascending = rows.clone();
    ascending.sort_by_key(|r| (r.val.is_none(), r.val, r.id));
    let want_asc: Vec<String> = ascending[..20]
        .iter()
        .map(|r| format!("{}\t{}", null_or(r.val), r.id))
        .collect();
    let mut descending = rows.clone();
    descending.sort_by_key(|r| (r.val.is_some(), std::cmp::Reverse(r.val), r.id));
    let want_desc: Vec<String> = descending[..20]
        .iter()
        .map(|r| format!("{}\t{}", null_or(r.val), r.id))
        .collect();

    let mut strings = rows.clone();
    strings.sort_by(|a, b| {
        (a.label.is_none(), &a.label, a.id).cmp(&(b.label.is_none(), &b.label, b.id))
    });
    let want_all: Vec<String> = strings
        .iter()
        .map(|r| {
            let name = r.label.as_deref().map_or("\\N".to_string(), escaped);
            format!("{name}\t{}", r.id)
        })
        .collect();

    for threads in ["1", "8"] {
        assert_eq!(
            sql(
                &db,
                threads,
                "SELECT grp, id FROM t ORDER BY grp DESC, id LIMIT 25 OFFSET 10"
            ),
            want
        );
        assert_eq!(
            sql(
                &db,
                threads,
                "SELECT val, id FROM t ORDER BY val, id LIMIT 20"
            ),
            want_asc
        );
        assert_eq!(
            sql(
                &db,
                threads,
                "SELECT val, id FROM t ORDER BY val DESC, id LIMIT 20"
            ),
            want_desc
        );
        assert_eq!(
            sql(&db, threads, "SELECT label, id FROM t ORDER BY label, id"),
            want_all
        );
        assert_eq!(
            sql(
                &db,
                threads,
                "SELECT id FROM t ORDER BY grp DESC, id LIMIT 1"
            )
            .len(),
            1
        );
    }
}

#[test]
fn like_and_filters_with_nulls() {
    let rows = generate_rows(11, 20_000);
    let db = loaded_database("filter", &rows);

    let count = |pred: &dyn Fn(&Row) -> bool| rows.iter().filter(|r| pred(r)).count().to_string();
    let al = count(&|r| r.label.as_deref().is_some_and(|l| l.starts_with("al")));
    let not_al = count(&|r| r.label.as_deref().is_some_and(|l| !l.starts_with("al")));
    let ta = count(&|r| r.label.as_deref().is_some_and(|l| l.contains("ta")));
    let high = count(&|r| r.val.is_some_and(|v| v > 100));
    let not_high = count(&|r| r.val.is_some_and(|v| v <= 100));
    let null_val = count(&|r| r.val.is_none());
    let mix = count(&|r| r.val.is_some_and(|v| v > 100) && r.label.is_none());
    let either = count(&|r| r.val.is_some_and(|v| v > 100) || r.val.is_none());

    for threads in ["1", "8"] {
        let one = |query: &str| sql(&db, threads, query).join("|");
        assert_eq!(one("SELECT count(*) FROM t WHERE label LIKE 'al%'"), al);
        assert_eq!(
            one("SELECT count(*) FROM t WHERE label NOT LIKE 'al%'"),
            not_al
        );
        assert_eq!(one("SELECT count(*) FROM t WHERE label LIKE '%ta%'"), ta);
        assert_eq!(one("SELECT count(*) FROM t WHERE val > 100"), high);
        assert_eq!(
            one("SELECT count(*) FROM t WHERE NOT (val > 100)"),
            not_high
        );
        assert_eq!(one("SELECT count(*) FROM t WHERE val IS NULL"), null_val);
        assert_eq!(
            one("SELECT count(*) FROM t WHERE val > 100 AND label IS NULL"),
            mix
        );
        assert_eq!(
            one("SELECT count(*) FROM t WHERE val > 100 OR val IS NULL"),
            either
        );
    }

    let first = rows
        .iter()
        .find(|r| r.val.is_some_and(|v| v > 100))
        .expect("some row passes");
    let projected = sql(
        &db,
        "4",
        &format!("SELECT id + 1, val FROM t WHERE id = {}", first.id),
    );
    assert_eq!(
        projected,
        vec![format!("{}\t{}", first.id + 1, null_or(first.val))]
    );
}

#[test]
fn format_stats_and_output_options() {
    let rows = generate_rows(13, 9000);
    let db = loaded_database("options", &rows);
    let db_text = text(&db);

    let header = veta(&[
        "sql",
        "--db",
        db_text,
        "--format",
        "tsv-header",
        "SELECT id AS x, count(*) FROM t WHERE id = 3 GROUP BY id",
    ]);
    assert_eq!(header.status.code(), Some(0), "{header:?}");
    assert_eq!(
        String::from_utf8(header.stdout).unwrap(),
        "x\tcount(*)\n3\t1\n"
    );

    let stats = veta(&[
        "sql",
        "--db",
        db_text,
        "--stats",
        "--no-optimizer",
        "--no-prune",
        "--join-order",
        "syntactic",
        "SELECT count(*) FROM t",
    ]);
    assert_eq!(stats.status.code(), Some(0), "{stats:?}");
    let stderr = String::from_utf8(stats.stderr).unwrap();
    let line = stderr
        .lines()
        .find(|l| l.starts_with("VETA_STATS {"))
        .expect("stats line");
    for key in [
        "parts_total",
        "parts_read",
        "granules_total",
        "granules_read",
        "row_groups_total",
        "row_groups_read",
    ] {
        assert!(line.contains(&format!("\"{key}\":")), "{line}");
    }
    assert!(line.contains("\"rows_read\":9000"), "{line}");
    assert!(line.contains("\"row_groups_total\":0"), "{line}");

    let parts = veta(&["parts", "--db", db_text, "--table", "t"]);
    assert_eq!(parts.status.code(), Some(0), "{parts:?}");
    let listing = String::from_utf8(parts.stdout).unwrap();
    let mut total = 0;
    for line in listing.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(fields.len(), 4, "{line}");
        assert!(fields[0].starts_with("all_"), "{line}");
        total += fields[1].parse::<u64>().unwrap();
        assert_eq!(fields[2], "0", "{line}");
        assert!(fields[3].parse::<u64>().unwrap() > 0, "{line}");
    }
    assert_eq!(total, 9000);

    let recover = veta(&["recover", "--db", db_text, "--table", "t"]);
    assert_eq!(recover.status.code(), Some(0), "{recover:?}");
    let recover_all = veta(&["recover", "--db", db_text]);
    assert_eq!(recover_all.status.code(), Some(0), "{recover_all:?}");
    let check = veta(&["check", "--db", db_text, "--table", "t"]);
    assert_eq!(check.status.code(), Some(0), "{check:?}");
}

#[test]
fn create_table_and_replace() {
    let dir = test_dir("create");
    let db = dir.join("db");
    let db_text = text(&db);
    let create = "CREATE TABLE r (a INTEGER)";
    assert_eq!(
        veta(&["sql", "--db", db_text, create]).status.code(),
        Some(0)
    );
    assert_eq!(
        veta(&["sql", "--db", db_text, create]).status.code(),
        Some(1)
    );
    assert_eq!(
        veta(&[
            "sql",
            "--db",
            db_text,
            "CREATE OR REPLACE TABLE r (a INTEGER, b VARCHAR)"
        ])
        .status
        .code(),
        Some(0)
    );
    assert_eq!(
        sql(&db, "2", "SELECT count(*), sum(a), max(b) FROM r"),
        vec!["0\t\\N\t\\N".to_string()]
    );
    let table_sql = fs::read_to_string(db.join("r").join("table.sql")).unwrap();
    assert!(!table_sql.contains("OR REPLACE"), "{table_sql}");
    assert_eq!(sql(&db, "2", "SELECT 1 + 2"), vec!["3".to_string()]);
}

/// A page of type DATA_PAGE (v1) without compression.
fn data_page(rows: i32, body: &[u8]) -> Vec<u8> {
    let mut out = encode(&PageHeader {
        type_: PageType::DataPage,
        uncompressed_page_size: body.len() as i32,
        compressed_page_size: body.len() as i32,
        crc: None,
        data_page_header: Some(DataPageHeader {
            num_values: rows,
            encoding: Encoding::Plain,
            definition_level_encoding: Encoding::Rle,
            repetition_level_encoding: Encoding::Rle,
            statistics: None,
        }),
        dictionary_page_header: None,
        data_page_header_v2: None,
    });
    out.extend_from_slice(body);
    out
}

fn leaf(name: &str, type_: Type, repetition: FieldRepetitionType) -> SchemaElement {
    SchemaElement {
        type_: Some(type_),
        type_length: None,
        repetition_type: Some(repetition),
        name: name.into(),
        num_children: None,
        converted_type: None,
        scale: None,
        precision: None,
        field_id: None,
        logical_type: None,
    }
}

/// A Parquet file of 5 rows with the columns `ID` (INT64), `Name` (BYTE_ARRAY), `Score`
/// (optional INT32 with [1, NULL, 3, NULL, 5]), `Days` and `Secs` (INT32) and an extra one.
fn minimal_parquet() -> Vec<u8> {
    let rows = 5i64;
    let mut ids = Vec::new();
    for v in [10i64, 20, 30, 40, 50] {
        ids.extend_from_slice(&v.to_le_bytes());
    }
    let mut names = Vec::new();
    for name in ["a", "bb", "", "dddd", "e"] {
        names.extend_from_slice(&(name.len() as u32).to_le_bytes());
        names.extend_from_slice(name.as_bytes());
    }
    let mut scores = 2u32.to_le_bytes().to_vec();
    scores.extend_from_slice(&[(1 << 1) | 1, 0b0001_0101]); // levels 1,0,1,0,1
    for v in [1i32, 3, 5] {
        scores.extend_from_slice(&v.to_le_bytes());
    }
    let int32s = |values: [i32; 5]| {
        let mut out = Vec::new();
        for v in values {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out
    };
    let columns = vec![
        (leaf("ID", Type::Int64, FieldRepetitionType::Required), ids),
        (
            leaf("Name", Type::ByteArray, FieldRepetitionType::Required),
            names,
        ),
        (
            leaf("Score", Type::Int32, FieldRepetitionType::Optional),
            scores,
        ),
        (
            leaf("Days", Type::Int32, FieldRepetitionType::Required),
            int32s([15_000, 15_001, 15_002, 15_003, 15_004]),
        ),
        (
            leaf("Secs", Type::Int32, FieldRepetitionType::Required),
            int32s([0, 60, 3600, 86_400, 86_460]),
        ),
        (
            leaf("Extra", Type::Int32, FieldRepetitionType::Required),
            int32s([9, 9, 9, 9, 9]),
        ),
    ];

    let mut out = MAGIC.to_vec();
    let mut schema = vec![SchemaElement {
        type_: None,
        type_length: None,
        repetition_type: None,
        name: "schema".into(),
        num_children: Some(columns.len() as i32),
        converted_type: None,
        scale: None,
        precision: None,
        field_id: None,
        logical_type: None,
    }];
    let mut chunks = Vec::new();
    for (element, body) in columns {
        let start = out.len() as i64;
        let page = data_page(rows as i32, &body);
        out.extend_from_slice(&page);
        chunks.push(ColumnChunk {
            file_path: None,
            file_offset: start,
            meta_data: Some(ColumnMetaData {
                type_: element.type_.expect("leaf type"),
                encodings: vec![Encoding::Plain, Encoding::Rle],
                path_in_schema: vec![element.name.clone()],
                codec: CompressionCodec::Uncompressed,
                num_values: rows,
                total_uncompressed_size: page.len() as i64,
                total_compressed_size: page.len() as i64,
                key_value_metadata: None,
                data_page_offset: start,
                index_page_offset: None,
                dictionary_page_offset: None,
                statistics: None,
            }),
        });
        schema.push(element);
    }
    let meta = FileMetaData {
        version: 2,
        schema,
        num_rows: rows,
        row_groups: vec![RowGroup {
            columns: chunks,
            total_byte_size: out.len() as i64,
            num_rows: rows,
            file_offset: Some(4),
            total_compressed_size: None,
            ordinal: Some(0),
        }],
        key_value_metadata: None,
        created_by: Some("veta test".into()),
        column_orders: None,
    };
    write_footer(&mut out, &meta).unwrap();
    out
}

#[test]
fn load_parquet_maps_columns_by_name_and_converts() {
    let dir = test_dir("parquet");
    let db = dir.join("db");
    let db_text = text(&db);
    let file = dir.join("data.parquet");
    fs::write(&file, minimal_parquet()).unwrap();
    let file_text = text(&file);

    let create = "CREATE TABLE p (name VARCHAR NOT NULL, id SMALLINT NOT NULL, score BIGINT, \
                  days DATE NOT NULL, secs TIMESTAMP NOT NULL, PRIMARY KEY (id))";
    assert_eq!(
        veta(&["sql", "--db", db_text, create]).status.code(),
        Some(0)
    );
    let load = veta(&[
        "load",
        "--db",
        db_text,
        "--table",
        "p",
        "--parquet",
        file_text,
    ]);
    assert_eq!(load.status.code(), Some(0), "{load:?}");

    assert_eq!(
        sql(
            &db,
            "2",
            "SELECT id, name, score, days, secs FROM p ORDER BY id"
        ),
        vec![
            "10\ta\t1\t2011-01-26\t1970-01-01 00:00:00".to_string(),
            "20\tbb\t\\N\t2011-01-27\t1970-01-01 00:01:00".to_string(),
            "30\t\t3\t2011-01-28\t1970-01-01 01:00:00".to_string(),
            "40\tdddd\t\\N\t2011-01-29\t1970-01-02 00:00:00".to_string(),
            "50\te\t5\t2011-01-30\t1970-01-02 00:01:00".to_string(),
        ]
    );

    // The same file, read directly.
    let direct = format!("SELECT sum(ID), count(Score) FROM read_parquet('{file_text}')");
    let stats = veta(&["sql", "--db", db_text, "--stats", &direct]);
    assert_eq!(stats.status.code(), Some(0), "{stats:?}");
    assert_eq!(String::from_utf8(stats.stdout).unwrap(), "150\t3\n");
    let stderr = String::from_utf8(stats.stderr).unwrap();
    assert!(stderr.contains("\"row_groups_total\":1"), "{stderr}");
    assert!(stderr.contains("\"row_groups_read\":1"), "{stderr}");
    assert!(stderr.contains("\"rows_read\":5"), "{stderr}");

    // NULL in a NOT NULL column and a value out of range are user errors.
    let strict = "CREATE TABLE q (score INTEGER NOT NULL)";
    assert_eq!(
        veta(&["sql", "--db", db_text, strict]).status.code(),
        Some(0)
    );
    let load = veta(&[
        "load",
        "--db",
        db_text,
        "--table",
        "q",
        "--parquet",
        file_text,
    ]);
    assert_eq!(load.status.code(), Some(1), "{load:?}");
    let narrow = "CREATE TABLE z (id SMALLINT, secs SMALLINT)";
    assert_eq!(
        veta(&["sql", "--db", db_text, narrow]).status.code(),
        Some(0)
    );
    let load = veta(&[
        "load",
        "--db",
        db_text,
        "--table",
        "z",
        "--parquet",
        file_text,
    ]);
    assert_eq!(load.status.code(), Some(1), "{load:?}");
    let missing = "CREATE TABLE y (nope INTEGER)";
    assert_eq!(
        veta(&["sql", "--db", db_text, missing]).status.code(),
        Some(0)
    );
    let load = veta(&[
        "load",
        "--db",
        db_text,
        "--table",
        "y",
        "--parquet",
        file_text,
    ]);
    assert_eq!(load.status.code(), Some(1), "{load:?}");
}

#[test]
fn invalid_sql_exits_with_1() {
    let dir = test_dir("invalid");
    let db = dir.join("db");
    let db_text = text(&db);
    for query in [
        "SELEC 1",
        "SELECT * FROM missing",
        "SELECT nope FROM (SELECT 1 AS a) AS s",
        "SELECT sum(DISTINCT a) FROM (SELECT 1 AS a) AS s",
    ] {
        let done = veta(&["sql", "--db", db_text, query]);
        assert_eq!(done.status.code(), Some(1), "{query}: {done:?}");
        let stderr = String::from_utf8(done.stderr).unwrap();
        assert!(stderr.starts_with("veta: "), "{stderr}");
    }
}

/// Changes the byte in the middle of `file` of a part of table `t`.
fn flip_byte(db: &Path, part: &str, file: &str) {
    let path = db.join("t").join(part).join(file);
    let mut bytes = fs::read(&path).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x55;
    fs::write(&path, bytes).unwrap();
}

fn assert_detached(db: &Path, part: &str) {
    assert!(!db.join("t").join(part).exists(), "{part} is still active");
    assert!(
        db.join("t")
            .join("detached")
            .join(format!("broken_{part}"))
            .is_dir(),
        "{part} is not in detached/"
    );
}

/// The one stderr line of a corrupt block: `veta: corrupt part <part>: <reason>`.
fn assert_corrupt_line(done: &Output, part: &str) {
    assert_eq!(done.status.code(), Some(3), "{done:?}");
    let stderr = String::from_utf8_lossy(&done.stderr);
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    let reason = stderr.strip_prefix(&format!("veta: corrupt part {part}: "));
    assert!(reason.is_some_and(|r| r.len() > 1), "{stderr}");
}

#[test]
fn corrupt_block_exits_with_3() {
    let rows = generate_rows(17, 12_000);
    let db = loaded_database("corrupt", &rows);
    let db_text = text(&db);
    flip_byte(&db, "all_2_2_0", "c0.bin");

    let done = veta(&["sql", "--db", db_text, "SELECT sum(id) FROM t"]);
    assert_corrupt_line(&done, "all_2_2_0");
    assert!(done.stdout.is_empty());
    assert_detached(&db, "all_2_2_0");

    let check = veta(&["check", "--db", db_text, "--table", "t"]);
    assert_eq!(check.status.code(), Some(0), "{check:?}");
}

#[test]
fn a_changed_byte_in_each_file_type_ends_with_the_part_out() {
    let rows = generate_rows(61, 12_000);
    let query = "SELECT sum(id), count(*) FROM t";
    // The second batch is the part that breaks.
    let kept: Vec<&Row> = rows[..4000].iter().chain(&rows[8000..]).collect();
    let without = format!(
        "{}\t{}",
        kept.iter().map(|r| r.id as i128).sum::<i128>(),
        kept.len()
    );
    for file in [
        "part.txt",
        "primary.idx",
        "c0.bin",
        "c0.mrk",
        "stats.bin",
        "sample.bin",
        "checksums.txt",
    ] {
        let db = loaded_database(&format!("corrupt-{}", file.replace('.', "-")), &rows);
        flip_byte(&db, "all_2_2_0", file);

        let done = veta(&["sql", "--db", text(&db), "--threads", "4", query]);
        let stderr = String::from_utf8_lossy(&done.stderr).into_owned();
        if file == "c0.bin" {
            assert_corrupt_line(&done, "all_2_2_0");
            assert!(done.stdout.is_empty(), "{done:?}");
        } else {
            assert_eq!(done.status.code(), Some(0), "{file}: {done:?}");
            assert_eq!(
                String::from_utf8_lossy(&done.stdout).trim_end(),
                without,
                "{file}"
            );
            assert!(
                stderr.starts_with("veta: detached broken part all_2_2_0: "),
                "{file}: {stderr}"
            );
        }
        assert_detached(&db, "all_2_2_0");

        assert_eq!(sql(&db, "4", query), vec![without.clone()], "{file}");
        let check = veta(&["check", "--db", text(&db), "--table", "t"]);
        assert_eq!(check.status.code(), Some(0), "{file}: {check:?}");
        fs::remove_dir_all(db.parent().unwrap()).unwrap();
    }
}

#[test]
fn check_detaches_every_corrupt_part_and_exits_with_3() {
    let rows = generate_rows(67, 12_000);
    let db = loaded_database("check-detaches", &rows);
    let db_text = text(&db);
    flip_byte(&db, "all_1_1_0", "stats.bin");
    flip_byte(&db, "all_3_3_0", "c2.bin");

    let check = veta(&["check", "--db", db_text, "--table", "t"]);
    assert_eq!(check.status.code(), Some(3), "{check:?}");
    let stderr = String::from_utf8_lossy(&check.stderr);
    assert!(
        stderr.contains("veta: detached broken part all_1_1_0: "),
        "{stderr}"
    );
    assert!(
        stderr.contains("veta: corrupt part all_3_3_0: "),
        "{stderr}"
    );
    assert_detached(&db, "all_1_1_0");
    assert_detached(&db, "all_3_3_0");
    let report = String::from_utf8_lossy(&check.stdout);
    assert!(report.starts_with("check: 3 parts, "), "{report}");
    assert!(report.trim_end().ends_with(", 2 corrupt"), "{report}");
    assert_eq!(active_parts(&db).len(), 1);

    let again = veta(&["check", "--db", db_text, "--table", "t"]);
    assert_eq!(again.status.code(), Some(0), "{again:?}");
    assert!(again.stderr.is_empty(), "{again:?}");
    fs::remove_dir_all(db.parent().unwrap()).unwrap();
}

#[test]
fn every_tool_that_reads_blocks_exits_with_3_and_detaches() {
    let rows = generate_rows(71, 12_000);
    let db = loaded_database("tools-corrupt", &rows);
    let db_text = text(&db);
    let out = db.parent().unwrap().join("dump");

    flip_byte(&db, "all_1_1_0", "c1.bin");
    let dump = veta(&[
        "dump-columns",
        "--db",
        db_text,
        "--table",
        "t",
        "--out",
        text(&out),
    ]);
    assert_corrupt_line(&dump, "all_1_1_0");
    assert_detached(&db, "all_1_1_0");

    flip_byte(&db, "all_2_2_0", "c3.bin");
    let roundtrip = veta(&["codec-roundtrip", "--db", db_text, "--table", "t"]);
    assert_corrupt_line(&roundtrip, "all_2_2_0");
    assert_detached(&db, "all_2_2_0");

    // The rest reads well, and a part broken on opening is reported and left out.
    let roundtrip = veta(&["codec-roundtrip", "--db", db_text, "--table", "t"]);
    assert_eq!(roundtrip.status.code(), Some(0), "{roundtrip:?}");
    flip_byte(&db, "all_3_3_0", "c0.mrk");
    let dump = veta(&[
        "dump-columns",
        "--db",
        db_text,
        "--table",
        "t",
        "--out",
        text(&out),
    ]);
    assert_eq!(dump.status.code(), Some(0), "{dump:?}");
    let stderr = String::from_utf8_lossy(&dump.stderr);
    assert!(
        stderr.starts_with("veta: detached broken part all_3_3_0: "),
        "{stderr}"
    );
    assert_detached(&db, "all_3_3_0");
    fs::remove_dir_all(db.parent().unwrap()).unwrap();
}

#[test]
fn explain_detaches_a_part_broken_on_opening() {
    let rows = generate_rows(73, 12_000);
    let db = loaded_database("explain-corrupt", &rows);
    flip_byte(&db, "all_2_2_0", "sample.bin");
    let done = veta(&["sql", "--db", text(&db), "EXPLAIN SELECT id FROM t"]);
    assert_eq!(done.status.code(), Some(0), "{done:?}");
    let stderr = String::from_utf8_lossy(&done.stderr);
    assert!(
        stderr.starts_with("veta: detached broken part all_2_2_0: "),
        "{stderr}"
    );
    assert_detached(&db, "all_2_2_0");
    fs::remove_dir_all(db.parent().unwrap()).unwrap();
}

/// What a `kill -9` may leave, all at once: a half-written insert with a dead owner, a merge
/// written and not renamed, and a half-deleted part. The merge sources stay in place, so the
/// active set is the one from before and the rows do not change.
fn leave_crash_state(db: &Path) {
    let dead = db.join("t").join("tmp_insert_90");
    fs::create_dir(&dead).unwrap();
    fs::write(dead.join("owner.lock"), b"").unwrap();
    fs::write(dead.join("c0.bin"), b"partial").unwrap();
    let merge = db.join("t").join("tmp_merge_all_1_2_1");
    fs::create_dir(&merge).unwrap();
    fs::write(merge.join("owner.lock"), b"").unwrap();
    let doomed = db.join("t").join("delete_tmp_all_80_80_0");
    fs::create_dir(&doomed).unwrap();
    fs::write(doomed.join("c0.bin"), b"partial").unwrap();
}

#[test]
fn recover_and_load_clean_what_a_crash_left() {
    let rows = generate_rows(79, 12_000);
    let db = loaded_database("recover-states", &rows);
    let db_text = text(&db);
    let query = "SELECT sum(id), count(*) FROM t";
    let before = sql(&db, "2", query);
    let parts = active_parts(&db);

    leave_crash_state(&db);
    assert_eq!(leftover_dirs(&db).len(), 3);
    // Readers never delete.
    assert_eq!(sql(&db, "2", query), before);
    assert_eq!(leftover_dirs(&db).len(), 3);

    let done = veta(&["recover", "--db", db_text, "--table", "t"]);
    assert_eq!(done.status.code(), Some(0), "{done:?}");
    assert!(leftover_dirs(&db).is_empty());
    assert_eq!(active_parts(&db), parts);
    assert_eq!(sql(&db, "2", query), before);

    // A part broken on opening is separated by recover, which still ends with 0.
    flip_byte(&db, "all_3_3_0", "part.txt");
    let done = veta(&["recover", "--db", db_text]);
    assert_eq!(done.status.code(), Some(0), "{done:?}");
    let stderr = String::from_utf8_lossy(&done.stderr);
    assert!(
        stderr.starts_with("veta: detached broken part all_3_3_0: "),
        "{stderr}"
    );
    assert_detached(&db, "all_3_3_0");

    // `load` starts by recovering.
    leave_crash_state(&db);
    let batch = db.parent().unwrap().join("batch0.tsv");
    let done = veta(&[
        "load",
        "--db",
        db_text,
        "--table",
        "t",
        "--tsv",
        text(&batch),
    ]);
    assert_eq!(done.status.code(), Some(0), "{done:?}");
    assert!(leftover_dirs(&db).is_empty());

    // So does `merge`.
    let check = veta(&["check", "--db", db_text, "--table", "t"]);
    assert_eq!(check.status.code(), Some(0), "{check:?}");
    leave_crash_state(&db);
    let done = veta(&["merge", "--db", db_text, "--table", "t", "--final"]);
    assert_eq!(done.status.code(), Some(0), "{done:?}");
    assert!(leftover_dirs(&db).is_empty());
    assert_eq!(active_parts(&db).len(), 1);
    fs::remove_dir_all(db.parent().unwrap()).unwrap();
}

#[test]
fn memory_limit_exits_with_4() {
    let rows = generate_rows(19, 20_000);
    let db = loaded_database("memory", &rows);
    let done = veta(&[
        "sql",
        "--db",
        text(&db),
        "--memory-limit-mb",
        "0",
        "SELECT id, count(*) FROM t GROUP BY id",
    ]);
    assert_eq!(done.status.code(), Some(4), "{done:?}");
}

#[test]
fn codec_tools_and_column_dump() {
    let random = veta(&["codec-roundtrip", "--random", "50", "--seed", "3"]);
    assert_eq!(random.status.code(), Some(0), "{random:?}");
    let stdout = String::from_utf8(random.stdout).unwrap();
    assert!(stdout.lines().any(|l| l.starts_with("OK vlz ")), "{stdout}");
    assert!(
        stdout.lines().any(|l| l.starts_with("OK snappy ")),
        "{stdout}"
    );
    let summary = stdout.lines().last().unwrap();
    assert!(summary.starts_with("codec-roundtrip: "), "{summary}");
    assert!(summary.ends_with(" cases, 0 failures"), "{summary}");

    let rows = generate_rows(23, 10_000);
    let db = loaded_database("dump", &rows);
    let db_text = text(&db);
    let stored = veta(&["codec-roundtrip", "--db", db_text, "--table", "t"]);
    assert_eq!(stored.status.code(), Some(0), "{stored:?}");
    let stdout = String::from_utf8(stored.stdout).unwrap();
    assert!(stdout.contains(" cases, 0 failures"), "{stdout}");
    assert!(stdout.lines().any(|l| l == "OK block label"), "{stdout}");

    let out = db.parent().unwrap().join("dump");
    let dump = veta(&[
        "dump-columns",
        "--db",
        db_text,
        "--table",
        "t",
        "--out",
        text(&out),
    ]);
    assert_eq!(dump.status.code(), Some(0), "{dump:?}");
    for (name, width) in [("id", 8), ("grp", 8), ("price", 8), ("day", 4)] {
        let bytes = fs::read(out.join(format!("{name}.raw"))).unwrap();
        assert_eq!(bytes.len(), rows.len() * width, "{name}");
    }
    let vals = rows.iter().filter(|r| r.val.is_some()).count();
    assert_eq!(fs::read(out.join("val.raw")).unwrap().len(), vals * 4);
    let labels: usize = rows
        .iter()
        .filter_map(|r| r.label.as_ref())
        .map(|l| 4 + l.len())
        .sum();
    assert_eq!(fs::read(out.join("label.raw")).unwrap().len(), labels);

    let bench = veta(&[
        "codec-bench",
        "--compressor",
        "vlz",
        "--file",
        text(&out.join("id.raw")),
        "--iterations",
        "3",
    ]);
    assert_eq!(bench.status.code(), Some(0), "{bench:?}");
    let line = String::from_utf8(bench.stdout).unwrap();
    assert!(line.starts_with("{\"file\":"), "{line}");
    for key in [
        "raw_bytes",
        "compressed_bytes",
        "compress_mb_s",
        "decompress_mb_s",
    ] {
        assert!(line.contains(&format!("\"{key}\":")), "{line}");
    }
    assert!(
        line.contains(&format!("\"raw_bytes\":{}", rows.len() * 8)),
        "{line}"
    );
}

/// The lines of `parts` for table `t`.
fn active_parts(db: &Path) -> Vec<String> {
    let done = veta(&["parts", "--db", text(db), "--table", "t"]);
    assert_eq!(done.status.code(), Some(0), "{done:?}");
    String::from_utf8(done.stdout)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

/// The directories of table `t` that are neither parts nor `detached`.
fn leftover_dirs(db: &Path) -> Vec<String> {
    fs::read_dir(db.join("t"))
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().into_string().unwrap())
        .filter(|name| !name.starts_with("all_") && name != "detached")
        .collect()
}

#[test]
fn merge_final_leaves_one_part_with_the_same_rows() {
    let rows = generate_rows(41, 30_000);
    let db = loaded_database("merge-final", &rows);
    let all = "SELECT * FROM t";
    let by_group = "SELECT grp, count(*), sum(val) FROM t GROUP BY grp ORDER BY grp";
    let before = sorted(sql(&db, "2", all));
    let groups = sql(&db, "2", by_group);
    assert_eq!(before.len(), rows.len());
    assert!(active_parts(&db).len() > 2);

    let done = veta(&["merge", "--db", text(&db), "--table", "t", "--final"]);
    assert_eq!(done.status.code(), Some(0), "{done:?}");
    let stdout = String::from_utf8(done.stdout).unwrap();
    assert!(stdout.ends_with("1 active parts\n"), "{stdout}");

    let parts = active_parts(&db);
    assert_eq!(parts.len(), 1, "{parts:?}");
    let fields: Vec<&str> = parts[0].split('\t').collect();
    assert_eq!(fields[1], rows.len().to_string());
    assert!(leftover_dirs(&db).is_empty());
    assert_eq!(sorted(sql(&db, "2", all)), before);
    assert_eq!(sql(&db, "2", by_group), groups);
    let check = veta(&["check", "--db", text(&db), "--table", "t"]);
    assert_eq!(check.status.code(), Some(0), "{check:?}");
    fs::remove_dir_all(db.parent().unwrap()).unwrap();
}

#[test]
fn merge_follows_its_limits() {
    let rows = generate_rows(42, 30_000);
    let db = loaded_database("merge-limits", &rows);
    let parts = active_parts(&db).len();

    let done = veta(&[
        "merge",
        "--db",
        text(&db),
        "--table",
        "t",
        "--max-merge-rows",
        "1",
    ]);
    assert_eq!(done.status.code(), Some(0), "{done:?}");
    assert_eq!(active_parts(&db).len(), parts);

    // `--final` ignores the row limit but not the memory limit.
    let done = veta(&[
        "merge",
        "--db",
        text(&db),
        "--table",
        "t",
        "--final",
        "--merge-memory-mb",
        "0",
    ]);
    assert_eq!(done.status.code(), Some(1), "{done:?}");
    assert_eq!(active_parts(&db).len(), parts);
    let done = veta(&[
        "merge",
        "--db",
        text(&db),
        "--table",
        "t",
        "--final",
        "--max-merge-rows",
        "1",
    ]);
    assert_eq!(done.status.code(), Some(0), "{done:?}");
    assert_eq!(active_parts(&db).len(), 1);
    fs::remove_dir_all(db.parent().unwrap()).unwrap();
}

#[test]
fn merge_recovers_first_and_a_corrupt_block_exits_with_3() {
    let rows = generate_rows(43, 30_000);
    let db = loaded_database("merge-corrupt", &rows);
    let dead = db.join("t").join("tmp_merge_all_1_1_9");
    fs::create_dir(&dead).unwrap();
    fs::write(dead.join("owner.lock"), b"").unwrap();

    let first = active_parts(&db)[0].split('\t').next().unwrap().to_string();
    let bin = db.join("t").join(&first).join("c0.bin");
    let mut bytes = fs::read(&bin).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0xff;
    fs::write(&bin, bytes).unwrap();

    let done = veta(&["merge", "--db", text(&db), "--table", "t", "--final"]);
    assert_eq!(done.status.code(), Some(3), "{done:?}");
    let stderr = String::from_utf8(done.stderr).unwrap();
    assert!(
        stderr.starts_with(&format!("veta: corrupt part {first}: ")),
        "{stderr}"
    );
    assert!(leftover_dirs(&db).is_empty());
    assert!(
        db.join("t/detached")
            .join(format!("broken_{first}"))
            .is_dir()
    );
    fs::remove_dir_all(db.parent().unwrap()).unwrap();
}

#[test]
fn the_background_merger_merges_every_table_until_it_is_killed() {
    let rows = generate_rows(44, 30_000);
    let db = loaded_database("merger", &rows);
    let all = "SELECT * FROM t";
    let before = sorted(sql(&db, "2", all));
    let done = veta(&[
        "sql",
        "--db",
        text(&db),
        "CREATE TABLE u (k BIGINT NOT NULL, PRIMARY KEY (k))",
    ]);
    assert_eq!(done.status.code(), Some(0), "{done:?}");
    let dir = db.parent().unwrap().to_path_buf();
    for (i, keys) in ["1\n2\n", "3\n4\n", "0\n5\n"].iter().enumerate() {
        let path = dir.join(format!("u{i}.tsv"));
        fs::write(&path, keys).unwrap();
        let done = veta(&[
            "load",
            "--db",
            text(&db),
            "--table",
            "u",
            "--tsv",
            text(&path),
        ]);
        assert_eq!(done.status.code(), Some(0), "{done:?}");
    }
    let parts_of = |table: &str| {
        let done = veta(&["parts", "--db", text(&db), "--table", table]);
        assert_eq!(done.status.code(), Some(0), "{done:?}");
        String::from_utf8(done.stdout).unwrap().lines().count()
    };
    assert!(parts_of("t") > 1 && parts_of("u") == 3);
    // What a dead merge left: the merger recovers before starting.
    let dead = db.join("t").join("tmp_merge_all_1_1_9");
    fs::create_dir(&dead).unwrap();
    fs::write(dead.join("owner.lock"), b"").unwrap();

    let mut merger = Command::new(env!("CARGO_BIN_EXE_veta"))
        .args(["merger", "--db", text(&db), "--interval-ms", "10"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the binary runs");
    let deadline = Instant::now() + Duration::from_secs(120);
    while (parts_of("t") > 1 || parts_of("u") > 1) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    // A merger only ends when it is killed.
    assert_eq!(merger.try_wait().unwrap(), None);
    merger.kill().unwrap();
    merger.wait().unwrap();
    // The kill may have caught the last cleanup.
    let done = veta(&["recover", "--db", text(&db)]);
    assert_eq!(done.status.code(), Some(0), "{done:?}");

    assert_eq!((parts_of("t"), parts_of("u")), (1, 1));
    assert!(leftover_dirs(&db).is_empty());
    assert_eq!(sorted(sql(&db, "2", all)), before);
    assert_eq!(
        sql(&db, "2", "SELECT k FROM u ORDER BY k"),
        vec!["0", "1", "2", "3", "4", "5"]
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn unsupported_subcommands_exit_with_1() {
    for name in ["export", "parquet-meta"] {
        let done = veta(&[name, "--db", "x"]);
        assert_eq!(done.status.code(), Some(1), "{name}");
        let stderr = String::from_utf8(done.stderr).unwrap();
        assert_eq!(stderr, format!("veta: unsupported: {name}\n"));
    }
}

/// `veta` run through `sh` with its soft limit of open files lowered to `soft`.
fn veta_with_soft_nofile(soft: u32, args: &[&str], raise: bool) -> Output {
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg("n=$1; shift; ulimit -S -n \"$n\" && exec \"$0\" \"$@\"")
        .arg(env!("CARGO_BIN_EXE_veta"))
        .arg(soft.to_string())
        .args(args);
    if raise {
        command.env_remove(veta_cli::fd_limit::REEXEC_MARK);
    } else {
        command.env(veta_cli::fd_limit::REEXEC_MARK, "1");
    }
    command.output().expect("the shell runs")
}

#[test]
fn reading_more_files_than_the_soft_limit_raises_it() {
    // 5 parts × 20 columns keep 100 `c<N>.bin` open (contract §6.3), above a soft limit of 64.
    let dir = test_dir("nofile");
    let db = dir.join("db");
    let columns: Vec<String> = (1..20).map(|c| format!("c{c} INTEGER")).collect();
    let create = format!(
        "CREATE TABLE w (k BIGINT NOT NULL, {}, PRIMARY KEY (k)) ENGINE = MergeTree",
        columns.join(", ")
    );
    let done = veta(&["sql", "--db", text(&db), &create]);
    assert_eq!(done.status.code(), Some(0), "{done:?}");
    let tsv = dir.join("w.tsv");
    let content: String = (0..50)
        .map(|k| {
            let cells: Vec<String> = (0..20).map(|c| (k * 100 + c).to_string()).collect();
            cells.join("\t") + "\n"
        })
        .collect();
    fs::write(&tsv, content).unwrap();
    let load = [
        "load",
        "--db",
        text(&db),
        "--table",
        "w",
        "--tsv",
        text(&tsv),
        "--batch-rows",
        "10",
    ];
    let done = veta(&load);
    assert_eq!(done.status.code(), Some(0), "{done:?}");

    let query = ["sql", "--db", text(&db), "SELECT * FROM w ORDER BY k"];
    let kept = veta_with_soft_nofile(64, &query, false);
    assert_ne!(kept.status.code(), Some(0), "{kept:?}");
    let stderr = String::from_utf8(kept.stderr).unwrap();
    assert!(stderr.contains("Too many open files"), "{stderr}");

    let raised = veta_with_soft_nofile(64, &query, true);
    assert_eq!(raised.status.code(), Some(0), "{raised:?}");
    let lines: Vec<String> = String::from_utf8(raised.stdout)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(lines.len(), 50);
    let first: Vec<String> = (0..20).map(|c| c.to_string()).collect();
    assert_eq!(lines[0], first.join("\t"));
}

/// Creates `name (id, k)` with `rows` and loads it.
fn join_table(dir: &Path, db: &Path, name: &str, rows: &[(i64, Option<i64>)]) {
    let create = format!("CREATE TABLE {name} (id BIGINT NOT NULL, k BIGINT, PRIMARY KEY (id))");
    let done = veta(&["sql", "--db", text(db), &create]);
    assert_eq!(done.status.code(), Some(0), "{done:?}");
    let tsv = dir.join(format!("{name}.tsv"));
    let lines: Vec<String> = rows
        .iter()
        .map(|(id, k)| match k {
            Some(k) => format!("{id}\t{k}\n"),
            None => format!("{id}\t\\N\n"),
        })
        .collect();
    fs::write(&tsv, lines.concat()).unwrap();
    let done = veta(&[
        "load",
        "--db",
        text(db),
        "--table",
        name,
        "--tsv",
        text(&tsv),
    ]);
    assert_eq!(done.status.code(), Some(0), "{done:?}");
}

#[test]
fn join_orders_and_explain_analyze() {
    let dir = test_dir("explain");
    let db = dir.join("db");
    join_table(
        &dir,
        &db,
        "big",
        &(0..200).map(|i| (i, Some(i % 5))).collect::<Vec<_>>(),
    );
    join_table(&dir, &db, "small", &[(0, Some(1)), (1, Some(3)), (2, None)]);
    let sql = "SELECT big.id, small.id FROM small, big WHERE small.k = big.k ORDER BY 1, 2";
    let run = |options: &[&str]| {
        let mut args = vec!["sql", "--db", text(&db)];
        args.extend_from_slice(options);
        args.push(sql);
        let out = veta(&args);
        assert_eq!(out.status.code(), Some(0), "{options:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap()
    };
    let by_cost = run(&[]);
    assert_eq!(by_cost.lines().count(), 80);
    assert_eq!(run(&["--join-order", "cost"]), by_cost);
    assert_eq!(run(&["--join-order", "syntactic"]), by_cost);
    assert_eq!(run(&["--no-optimizer"]), by_cost);

    let json = run(&["--explain-analyze"]);
    assert_eq!(json.lines().count(), 1, "{json}");
    assert!(json.starts_with("{\"op\":\""), "{json}");
    assert!(json.trim_end().ends_with('}'), "{json}");
    assert!(
        json.contains("\"op\":\"HashJoin\",\"kind\":\"inner\""),
        "{json}"
    );
    assert!(json.contains("\"actual_rows\":80"), "{json}");
    assert!(
        json.contains("{\"alias\":\"big\",\"table\":\"big\"}"),
        "{json}"
    );
    assert!(json.contains("\"(big.k = small.k)\""), "{json}");

    let statement = format!("EXPLAIN ANALYZE {sql}");
    let keyword = veta(&["sql", "--db", text(&db), &statement]);
    assert_eq!(keyword.status.code(), Some(0), "{keyword:?}");
    let keyword = String::from_utf8(keyword.stdout).unwrap();
    assert_eq!(keyword.lines().count(), 1, "{keyword}");
    assert!(keyword.contains("\"actual_rows\":80"), "{keyword}");

    let statement = format!("EXPLAIN {sql}");
    let plain = veta(&["sql", "--db", text(&db), &statement]);
    assert_eq!(plain.status.code(), Some(0), "{plain:?}");
    let plain = String::from_utf8(plain.stdout).unwrap();
    assert!(plain.contains("\"actual_rows\":null"), "{plain}");
    assert!(!plain.contains("\"actual_rows\":80"), "{plain}");
}
