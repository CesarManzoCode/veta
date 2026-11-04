//! `veta load` (contract §13): Parquet and TSV files into a table.
//!
//! Each file is a batch, or several of `--batch-rows` rows, and each batch one atomic part.
//! Parquet columns are found by name without regard to case, and only the conversions of
//! §13 are accepted. Recovery (§6.5) runs before anything is inserted.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use veta_core::format::{parse_value, split_fields};
use veta_core::{Chunk, DataType, Result, Vector, VectorData, VetaError};
use veta_parquet::ParquetFile;
use veta_storage::{Table, insert, recover};

use crate::database::Database;
use crate::expr_eval::cast_vector;

/// Rows of the chunks built while reading a TSV file.
const TSV_CHUNK_ROWS: usize = 8192;

/// The files of a `load`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadInput {
    Parquet(Vec<PathBuf>),
    Tsv(Vec<PathBuf>),
}

/// What a `load` inserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LoadSummary {
    pub parts: u64,
    pub rows: u64,
}

/// Splits the rows of a file into batches and inserts each one.
struct Batcher<'a> {
    table: &'a Table,
    batch_rows: Option<usize>,
    pending: Chunk,
    summary: LoadSummary,
}

impl Batcher<'_> {
    fn insert_batch(&mut self, batch: &Chunk) -> Result<()> {
        if insert(self.table, batch)?.is_some() {
            self.summary.parts += 1;
            self.summary.rows += batch.len as u64;
        }
        Ok(())
    }

    fn push(&mut self, chunk: &Chunk) -> Result<()> {
        self.pending.extend(chunk)?;
        if let Some(batch_rows) = self.batch_rows {
            while self.pending.len >= batch_rows {
                let batch = self.pending.slice(0, batch_rows);
                let rest = self
                    .pending
                    .slice(batch_rows, self.pending.len - batch_rows);
                self.insert_batch(&batch)?;
                self.pending = rest;
            }
        }
        Ok(())
    }

    /// The end of a file ends its last batch.
    fn finish_file(&mut self) -> Result<()> {
        let empty = Chunk::empty(&self.table.schema().data_types());
        let batch = std::mem::replace(&mut self.pending, empty);
        self.insert_batch(&batch)
    }
}

/// Converts a Parquet column to its table column: integers to integers (checking the
/// range), to `Date` (days) and to `Timestamp` (seconds), decimals of equal scale, and
/// nothing else (contract §13).
fn convert_column(v: Vector, to: DataType, name: &str) -> Result<Vector> {
    let from = v.data_type();
    if from == to {
        return Ok(v);
    }
    let reject = || VetaError::user(format!("cannot load column {name}: {from} into {to}"));
    match (from, to) {
        (f, t) if f.is_integer() && t.is_integer() => cast_vector(&v, t),
        (f, DataType::Date) if f.is_integer() => {
            let days = cast_vector(&v, DataType::Int32)?;
            let VectorData::Int32(values) = days.data else {
                return Err(reject());
            };
            Vector::new(VectorData::Date(values), days.validity)
        }
        (f, DataType::Timestamp) if f.is_integer() => {
            let seconds = cast_vector(&v, DataType::Int64)?;
            let VectorData::Int64(values) = seconds.data else {
                return Err(reject());
            };
            Vector::new(VectorData::Timestamp(values), seconds.validity)
        }
        (DataType::Decimal { scale: a, .. }, DataType::Decimal { scale: b, .. }) if a == b => {
            cast_vector(&v, to)
        }
        _ => Err(reject()),
    }
}

/// Schema index in `file` of each table column.
fn map_columns(table: &Table, file: &ParquetFile, path: &Path) -> Result<Vec<usize>> {
    table
        .schema()
        .fields
        .iter()
        .map(|field| {
            file.schema()
                .index_of_ignore_case(&field.name)
                .ok_or_else(|| {
                    VetaError::user(format!(
                        "column {} not found in {}",
                        field.name,
                        path.display()
                    ))
                })
        })
        .collect()
}

fn load_parquet_file(path: &Path, batcher: &mut Batcher<'_>) -> Result<()> {
    let file = ParquetFile::open(path)?;
    let columns = map_columns(batcher.table, &file, path)?;
    let fields = batcher.table.schema().fields.clone();
    for chunk in file.chunks(&columns)? {
        let chunk = chunk?;
        let len = chunk.len;
        let converted = chunk
            .columns
            .into_iter()
            .zip(&fields)
            .map(|(vector, field)| convert_column(vector, field.data_type, &field.name))
            .collect::<Result<Vec<Vector>>>()?;
        batcher.push(&Chunk::with_len(converted, len)?)?;
    }
    batcher.finish_file()
}

fn load_tsv_file(path: &Path, batcher: &mut Batcher<'_>) -> Result<()> {
    let file = File::open(path).map_err(|e| VetaError::Io(format!("{}: {e}", path.display())))?;
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let types = batcher.table.schema().data_types();
    let new_columns = || -> Vec<Vector> {
        types
            .iter()
            .map(|&t| Vector::with_capacity(t, TSV_CHUNK_ROWS))
            .collect()
    };
    let mut columns = new_columns();
    let mut rows = 0;
    let mut line = Vec::new();
    let mut number = 0u64;
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        number += 1;
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        let fields = split_fields(&line);
        if fields.len() != types.len() {
            return Err(VetaError::user(format!(
                "{}:{number}: expected {} fields, found {}",
                path.display(),
                types.len(),
                fields.len()
            )));
        }
        for ((column, field), &data_type) in columns.iter_mut().zip(fields).zip(&types) {
            column.push(&parse_value(field, data_type)?)?;
        }
        rows += 1;
        if rows == TSV_CHUNK_ROWS {
            let full = std::mem::replace(&mut columns, new_columns());
            batcher.push(&Chunk::with_len(full, rows)?)?;
            rows = 0;
        }
    }
    if rows > 0 {
        batcher.push(&Chunk::with_len(columns, rows)?)?;
    }
    batcher.finish_file()
}

/// Loads `input` into table `table_name` of `db`, after recovering the table.
pub fn load(
    db: &Database,
    table_name: &str,
    input: &LoadInput,
    batch_rows: Option<usize>,
) -> Result<LoadSummary> {
    if batch_rows == Some(0) {
        return Err(VetaError::user("--batch-rows must be at least 1"));
    }
    let table = db.open_table(table_name)?;
    let report = recover(&table)?;
    for detached in &report.detached {
        eprintln!("{}", detached.message());
    }
    let mut batcher = Batcher {
        table: &table,
        batch_rows,
        pending: Chunk::empty(&table.schema().data_types()),
        summary: LoadSummary::default(),
    };
    match input {
        LoadInput::Parquet(files) => {
            for path in files {
                load_parquet_file(path, &mut batcher)?;
            }
        }
        LoadInput::Tsv(files) => {
            for path in files {
                load_tsv_file(path, &mut batcher)?;
            }
        }
    }
    Ok(batcher.summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use veta_core::Value;
    use veta_sql::{Statement, parse_statement};
    use veta_storage::Snapshot;

    fn temp_db(tag: &str) -> (Database, PathBuf) {
        let dir = std::env::temp_dir().join(format!("veta-load-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        (Database::open(dir.join("db")), dir)
    }

    fn create(db: &Database, sql: &str) {
        let Statement::CreateTable(create) = parse_statement(sql).unwrap() else {
            panic!("not a CREATE TABLE");
        };
        db.create_table(&create).unwrap();
    }

    fn part_rows(db: &Database, name: &str) -> Vec<u64> {
        let table = db.open_table(name).unwrap();
        Snapshot::take(&table, Some(&[]))
            .unwrap()
            .parts()
            .iter()
            .map(|p| p.rows())
            .collect()
    }

    #[test]
    fn integers_convert_with_range_checks() {
        let small = Vector::from_data(VectorData::Int64(vec![1, 2]));
        let out = convert_column(small, DataType::Int16, "c").unwrap();
        assert_eq!(out.get(1), Value::Int16(2));
        let big = Vector::from_data(VectorData::Int64(vec![1 << 40]));
        let err = convert_column(big, DataType::Int16, "c").unwrap_err();
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn integers_become_dates_and_timestamps() {
        let days = Vector::from_data(VectorData::Int32(vec![19_000]));
        let out = convert_column(days, DataType::Date, "d").unwrap();
        assert_eq!(out.get(0), Value::Date(19_000));
        let seconds =
            Vector::from_values(DataType::Int32, &[Value::Int32(5), Value::Null]).unwrap();
        let out = convert_column(seconds, DataType::Timestamp, "t").unwrap();
        assert_eq!(out.get(0), Value::Timestamp(5));
        assert_eq!(out.get(1), Value::Null);
        let wide = Vector::from_data(VectorData::Int64(vec![1 << 40]));
        assert_eq!(
            convert_column(wide, DataType::Date, "d")
                .unwrap_err()
                .exit_code(),
            1
        );
    }

    #[test]
    fn decimals_need_the_same_scale_and_other_conversions_are_rejected() {
        let decimal = || {
            Vector::from_data(VectorData::Decimal64 {
                values: vec![1234],
                precision: 15,
                scale: 2,
            })
        };
        let same_scale = DataType::Decimal {
            precision: 20,
            scale: 2,
        };
        let out = convert_column(decimal(), same_scale, "x").unwrap();
        assert_eq!(out.data_type(), same_scale);
        let other_scale = DataType::Decimal {
            precision: 20,
            scale: 3,
        };
        assert_eq!(
            convert_column(decimal(), other_scale, "x")
                .unwrap_err()
                .exit_code(),
            1
        );
        let text = Vector::from_values(DataType::Varchar, &[Value::varchar("1")]).unwrap();
        assert_eq!(
            convert_column(text, DataType::Int64, "x")
                .unwrap_err()
                .exit_code(),
            1
        );
    }

    #[test]
    fn tsv_files_are_one_batch_or_split_in_batches() {
        let (db, dir) = temp_db("tsv");
        create(&db, "CREATE TABLE t (a BIGINT NOT NULL, s VARCHAR)");
        let tsv = dir.join("rows.tsv");
        let mut text = String::new();
        for i in 0..25 {
            text.push_str(&format!("{i}\tv\\t{i}\n"));
        }
        text.push_str("25\t\\N");
        fs::write(&tsv, text).unwrap();

        let input = LoadInput::Tsv(vec![tsv.clone()]);
        let whole = load(&db, "t", &input, None).unwrap();
        assert_eq!(whole, LoadSummary { parts: 1, rows: 26 });
        assert_eq!(part_rows(&db, "t"), vec![26]);

        create(
            &db,
            "CREATE OR REPLACE TABLE t (a BIGINT NOT NULL, s VARCHAR)",
        );
        let split = load(&db, "t", &input, Some(10)).unwrap();
        assert_eq!(split, LoadSummary { parts: 3, rows: 26 });
        assert_eq!(part_rows(&db, "t"), vec![10, 10, 6]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn bad_input_is_a_user_error_and_inserts_nothing() {
        let (db, dir) = temp_db("bad");
        create(&db, "CREATE TABLE t (a BIGINT NOT NULL, s VARCHAR)");
        let cases = [
            ("short.tsv", "1\n"),
            ("type.tsv", "x\ty\n"),
            ("null.tsv", "\\N\ty\n"),
        ];
        for (name, content) in cases {
            let path = dir.join(name);
            fs::write(&path, content).unwrap();
            let err = load(&db, "t", &LoadInput::Tsv(vec![path]), None).unwrap_err();
            assert_eq!(err.exit_code(), 1, "{name}: {err}");
        }
        assert!(part_rows(&db, "t").is_empty());
        assert_eq!(
            load(&db, "t", &LoadInput::Tsv(Vec::new()), Some(0))
                .unwrap_err()
                .exit_code(),
            1
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}
