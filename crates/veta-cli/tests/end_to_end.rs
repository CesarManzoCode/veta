//! End-to-end tests of the `veta` binary: a database in `CARGO_TARGET_TMPDIR` loaded from
//! generated TSV in several batches, with each result compared against the same result
//! computed by hand in Rust.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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

#[test]
fn corrupt_block_exits_with_3() {
    let rows = generate_rows(17, 12_000);
    let db = loaded_database("corrupt", &rows);
    let db_text = text(&db);
    let part = db.join("t").join("all_2_2_0");
    let bin = part.join("c0.bin");
    let mut bytes = fs::read(&bin).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x55;
    fs::write(&bin, bytes).unwrap();

    let check = veta(&["check", "--db", db_text, "--table", "t"]);
    assert_eq!(check.status.code(), Some(3), "{check:?}");
    let stderr = String::from_utf8(check.stderr).unwrap();
    assert!(
        stderr.contains("veta: corrupt part all_2_2_0: "),
        "{stderr}"
    );
    assert!(part.is_dir(), "check does not separate parts yet");

    let done = veta(&["sql", "--db", db_text, "SELECT sum(id) FROM t"]);
    assert_eq!(done.status.code(), Some(3), "{done:?}");
    assert!(done.stdout.is_empty());
    let stderr = String::from_utf8(done.stderr).unwrap();
    assert!(
        stderr.contains("veta: corrupt part all_2_2_0: "),
        "{stderr}"
    );
    assert!(!part.exists());
    assert!(
        db.join("t")
            .join("detached")
            .join("broken_all_2_2_0")
            .is_dir()
    );

    let check = veta(&["check", "--db", db_text, "--table", "t"]);
    assert_eq!(check.status.code(), Some(0), "{check:?}");
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

#[test]
fn unsupported_subcommands_exit_with_1() {
    for name in ["merge", "merger", "export", "parquet-meta"] {
        let done = veta(&[name, "--db", "x"]);
        assert_eq!(done.status.code(), Some(1), "{name}");
        let stderr = String::from_utf8(done.stderr).unwrap();
        assert_eq!(stderr, format!("veta: unsupported: {name}\n"));
    }
}
