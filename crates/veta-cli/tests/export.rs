//! `veta export` and `veta parquet-meta` end to end (contract §9, §13): a table loaded from
//! TSV goes out to Parquet and comes back through `read_parquet`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use veta_core::datetime::format_date;

const CREATE: &str = "CREATE TABLE t (id BIGINT NOT NULL, small SMALLINT, num INTEGER, \
                      price DECIMAL(10,2) NOT NULL, day DATE NOT NULL, ts TIMESTAMP, \
                      ratio DOUBLE, ok BOOLEAN, label VARCHAR, PRIMARY KEY (id)) \
                      ENGINE = MergeTree";

fn veta(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_veta"))
        .args(args)
        .output()
        .expect("the binary runs")
}

fn succeed(args: &[&str]) -> String {
    let done = veta(args);
    assert_eq!(done.status.code(), Some(0), "{args:?}: {done:?}");
    String::from_utf8(done.stdout).unwrap()
}

/// An empty directory for one test.
fn test_dir(tag: &str) -> PathBuf {
    let dir =
        Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("export-{tag}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn text(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn tsv_line(i: i64) -> String {
    let nullable = |text: String| {
        if i % 10 == 3 { "\\N".to_string() } else { text }
    };
    let cents = i * 37 % 100_000;
    format!(
        "{i}\t{}\t{}\t{}.{:02}\t{}\t{}\t{}\t{}\t{}\n",
        nullable((i % 500 - 250).to_string()),
        nullable((i * 1000).to_string()),
        cents / 100,
        cents % 100,
        format_date(18_000 + (i / 10) as i32),
        nullable("2020-05-06 07:08:09".to_string()),
        nullable((i as f64 / 8.0).to_string()),
        nullable((i % 2 == 0).to_string()),
        nullable(format!("label-{}", i % 40)),
    )
}

fn database(tag: &str, rows: i64) -> (PathBuf, PathBuf) {
    let dir = test_dir(tag);
    let db = dir.join("db");
    succeed(&["sql", "--db", text(&db), CREATE]);
    let tsv = dir.join("rows.tsv");
    fs::write(&tsv, (0..rows).map(tsv_line).collect::<String>()).unwrap();
    succeed(&[
        "load",
        "--db",
        text(&db),
        "--table",
        "t",
        "--tsv",
        text(&tsv),
    ]);
    (dir, db)
}

/// `VETA_STATS` field of a query run with `--stats`.
fn stat(done: &Output, key: &str) -> u64 {
    let stderr = String::from_utf8_lossy(&done.stderr);
    let line = stderr
        .lines()
        .find(|l| l.starts_with("VETA_STATS "))
        .expect("a stats line");
    let tail = line.split(&format!("\"{key}\":")).nth(1).expect("the key");
    let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().unwrap()
}

#[test]
fn what_export_writes_reads_back_through_read_parquet() {
    let (dir, db) = database("roundtrip", 5_000);
    let order = "SELECT * FROM t ORDER BY id";
    let source = succeed(&["sql", "--db", text(&db), order]);
    assert_eq!(source.lines().count(), 5_000);
    for encoding in ["plain", "dict", "delta", "delta-byte-array"] {
        for compression in ["none", "snappy"] {
            for page_version in ["1", "2"] {
                let out = dir.join(format!("{encoding}-{compression}-{page_version}.parquet"));
                succeed(&[
                    "export",
                    "--db",
                    text(&db),
                    "--sql",
                    order,
                    "--out",
                    text(&out),
                    "--encoding",
                    encoding,
                    "--compression",
                    compression,
                    "--page-version",
                    page_version,
                    "--row-group-rows",
                    "1200",
                ]);
                let back = format!("SELECT * FROM read_parquet('{}')", text(&out));
                assert_eq!(
                    succeed(&["sql", "--db", text(&db), &back]),
                    source,
                    "{encoding} {compression} {page_version}"
                );
                let meta = succeed(&["parquet-meta", "--file", text(&out)]);
                // Five row groups of nine columns each.
                assert_eq!(meta.lines().count(), 45);
            }
        }
    }
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn defaults_are_those_of_the_contract() {
    let (dir, db) = database("defaults", 10);
    let out = dir.join("x.parquet");
    succeed(&[
        "export",
        "--db",
        text(&db),
        "--sql",
        "SELECT id FROM t",
        "--out",
        text(&out),
    ]);
    let meta = succeed(&["parquet-meta", "--file", text(&out)]);
    assert_eq!(meta, "0\tid\tINT64\tPLAIN\tSNAPPY\t0\t9\t0\n");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn parquet_meta_prints_the_exact_table() {
    let dir = test_dir("meta");
    let db = dir.join("db");
    succeed(&[
        "sql",
        "--db",
        text(&db),
        "CREATE TABLE m (id BIGINT NOT NULL, s VARCHAR, d DATE NOT NULL, ts TIMESTAMP NOT NULL, \
         p DECIMAL(6,2) NOT NULL, b BOOLEAN NOT NULL, r DOUBLE NOT NULL, PRIMARY KEY (id)) \
         ENGINE = MergeTree",
    ]);
    let tsv = dir.join("m.tsv");
    fs::write(
        &tsv,
        "1\tb\\tc\t2020-01-02\t2020-01-02 03:04:05\t1.50\ttrue\t0.5\n\
         2\ta\t2020-01-01\t2020-01-02 03:04:06\t-2.25\tfalse\t2.5\n\
         3\t\\N\t2021-06-07\t1969-12-31 23:59:59\t0.00\ttrue\t-1\n",
    )
    .unwrap();
    succeed(&[
        "load",
        "--db",
        text(&db),
        "--table",
        "m",
        "--tsv",
        text(&tsv),
    ]);
    let out = dir.join("m.parquet");
    succeed(&[
        "export",
        "--db",
        text(&db),
        "--sql",
        "SELECT * FROM m ORDER BY id",
        "--out",
        text(&out),
        "--encoding",
        "dict",
        "--compression",
        "none",
        "--row-group-rows",
        "2",
    ]);
    let expected = "\
0\tid\tINT64\tPLAIN,RLE_DICTIONARY\tUNCOMPRESSED\t1\t2\t0
0\ts\tBYTE_ARRAY\tPLAIN,RLE,RLE_DICTIONARY\tUNCOMPRESSED\ta\tb\\tc\t0
0\td\tINT32\tPLAIN,RLE_DICTIONARY\tUNCOMPRESSED\t2020-01-01\t2020-01-02\t0
0\tts\tINT64\tPLAIN,RLE_DICTIONARY\tUNCOMPRESSED\t2020-01-02 03:04:05\t2020-01-02 03:04:06\t0
0\tp\tINT64\tPLAIN,RLE_DICTIONARY\tUNCOMPRESSED\t-2.25\t1.50\t0
0\tb\tBOOLEAN\tPLAIN,RLE_DICTIONARY\tUNCOMPRESSED\tfalse\ttrue\t0
0\tr\tDOUBLE\tPLAIN,RLE_DICTIONARY\tUNCOMPRESSED\t0.5\t2.5\t0
1\tid\tINT64\tPLAIN,RLE_DICTIONARY\tUNCOMPRESSED\t3\t3\t0
1\ts\tBYTE_ARRAY\tPLAIN,RLE\tUNCOMPRESSED\t\\N\t\\N\t1
1\td\tINT32\tPLAIN,RLE_DICTIONARY\tUNCOMPRESSED\t2021-06-07\t2021-06-07\t0
1\tts\tINT64\tPLAIN,RLE_DICTIONARY\tUNCOMPRESSED\t1969-12-31 23:59:59\t1969-12-31 23:59:59\t0
1\tp\tINT64\tPLAIN,RLE_DICTIONARY\tUNCOMPRESSED\t0.00\t0.00\t0
1\tb\tBOOLEAN\tPLAIN,RLE_DICTIONARY\tUNCOMPRESSED\ttrue\ttrue\t0
1\tr\tDOUBLE\tPLAIN,RLE_DICTIONARY\tUNCOMPRESSED\t-1\t-1\t0
";
    assert_eq!(succeed(&["parquet-meta", "--file", text(&out)]), expected);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn filters_skip_the_row_groups_export_wrote() {
    let (dir, db) = database("skip", 5_000);
    let out = dir.join("sorted.parquet");
    succeed(&[
        "export",
        "--db",
        text(&db),
        "--sql",
        "SELECT id, day FROM t ORDER BY day, id",
        "--out",
        text(&out),
        "--row-group-rows",
        "500",
    ]);
    let file = text(&out);
    // `day` grows by one every 10 rows, so each row group spans 50 days.
    let last_group_day = format_date(18_000 + 490);
    for (condition, read) in [
        (format!("day >= '{last_group_day}'"), 1),
        (
            format!("day > CAST('{}' AS DATE)", format_date(18_000 + 499)),
            0,
        ),
        (format!("day = '{}'", format_date(18_000)), 1),
        (format!("day < '{}'", format_date(18_000 + 75)), 2),
        ("id >= 4500".to_string(), 1),
        ("id < 0".to_string(), 0),
    ] {
        let sql = format!("SELECT count(*), sum(id) FROM read_parquet('{file}') WHERE {condition}");
        let pruned = veta(&["sql", "--db", text(&db), "--stats", &sql]);
        assert_eq!(pruned.status.code(), Some(0), "{sql}: {pruned:?}");
        let full = veta(&["sql", "--db", text(&db), "--stats", "--no-prune", &sql]);
        assert_eq!(full.status.code(), Some(0), "{sql}: {full:?}");
        assert_eq!(pruned.stdout, full.stdout, "{sql}");
        assert_eq!(stat(&pruned, "row_groups_total"), 10, "{sql}");
        assert_eq!(stat(&pruned, "row_groups_read"), read, "{sql}");
        assert_eq!(stat(&full, "row_groups_read"), 10, "{sql}");
    }
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn a_failed_export_leaves_nothing_new_behind() {
    let (dir, db) = database("errors", 10);
    let out = dir.join("kept.parquet");
    fs::write(&out, "before").unwrap();
    for (sql, code) in [
        ("SELECT sum(id) FROM t", 1),
        ("SELECT sum(price) FROM t", 1),
        ("SELECT id, id FROM t", 1),
        ("SELECT no_such_column FROM t", 1),
    ] {
        let done = veta(&[
            "export",
            "--db",
            text(&db),
            "--sql",
            sql,
            "--out",
            text(&out),
        ]);
        assert_eq!(done.status.code(), Some(code), "{sql}: {done:?}");
    }
    assert_eq!(fs::read(&out).unwrap(), b"before");
    let names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.starts_with("kept"))
        .collect();
    assert_eq!(names, ["kept.parquet"]);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn export_of_no_rows_writes_a_readable_file() {
    let (dir, db) = database("empty", 10);
    let out = dir.join("none.parquet");
    succeed(&[
        "export",
        "--db",
        text(&db),
        "--sql",
        "SELECT id, label FROM t WHERE id < 0",
        "--out",
        text(&out),
    ]);
    assert_eq!(succeed(&["parquet-meta", "--file", text(&out)]), "");
    let back = format!("SELECT count(*) FROM read_parquet('{}')", text(&out));
    assert_eq!(succeed(&["sql", "--db", text(&db), &back]), "0\n");
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn parquet_meta_of_a_bad_file_fails() {
    let dir = test_dir("bad");
    let junk = dir.join("junk.parquet");
    fs::write(&junk, "this is not a parquet file").unwrap();
    assert_eq!(
        veta(&["parquet-meta", "--file", text(&junk)]).status.code(),
        Some(1)
    );
    let missing = dir.join("missing.parquet");
    assert_eq!(
        veta(&["parquet-meta", "--file", text(&missing)])
            .status
            .code(),
        Some(2)
    );
    let _ = fs::remove_dir_all(dir);
}
