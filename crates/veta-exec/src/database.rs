//! A database: the `--db` directory with one subdirectory per table (contract §5.1).
//!
//! `table.sql` holds the canonical `CREATE TABLE`, which is parsed back into the
//! [`TableDef`] of `veta-storage` whenever a table is opened. [`Database`] is also the
//! catalog the binder resolves names against: it hands out the merged statistics of the
//! active parts and measures predicate selectivities on their samples (§11.5).

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use veta_core::stats::TableStats;
use veta_core::{Field, Result, Schema, VetaError};
use veta_parquet::ParquetFile;
use veta_sql::ast::CreateTable;
use veta_sql::{BoundExpr, Catalog, CatalogTable, Statement, parse_statement};
use veta_storage::part::{PartName, active_parts, list_parts};
use veta_storage::stats::merge_part_stats;
use veta_storage::table::{TABLE_SQL, table_exists};
use veta_storage::{PartReader, Snapshot, Table, TableDef};

use crate::expr_eval::eval_predicate;

/// Statistics of a table together with the active part set they were read from.
type CachedStats = (Vec<PartName>, Arc<TableStats>);

/// The directory of `--db`; it may not exist until the first `CREATE TABLE`.
#[derive(Debug, Clone)]
pub struct Database {
    dir: PathBuf,
    /// Merged statistics per lower-case table name; reused while the active parts stay
    /// the same.
    stats: Arc<Mutex<HashMap<String, CachedStats>>>,
}

/// Builds the definition of a table from its parsed `CREATE TABLE`: columns with their
/// nullability, the primary key resolved to schema indices and the canonical DDL (never
/// with `OR REPLACE`).
pub fn table_def(create: &CreateTable) -> Result<TableDef> {
    let schema = Schema::new(
        create
            .columns
            .iter()
            .map(|c| Field::new(c.name.clone(), c.data_type, c.nullable))
            .collect(),
    );
    let mut primary_key = Vec::with_capacity(create.primary_key.len());
    for name in &create.primary_key {
        let index = schema.index_of_ignore_case(name).ok_or_else(|| {
            VetaError::user(format!(
                "primary key column {name} is not a column of {}",
                create.name
            ))
        })?;
        primary_key.push(index);
    }
    let mut canonical = create.clone();
    canonical.or_replace = false;
    Ok(TableDef {
        name: create.name.clone(),
        schema,
        primary_key,
        ddl: canonical.to_string(),
    })
}

/// Reads the `CREATE TABLE` stored in a `table.sql`.
fn parse_ddl(ddl: &str) -> Result<TableDef> {
    match parse_statement(ddl)? {
        Statement::CreateTable(create) => table_def(&create),
        _ => Err(VetaError::internal("table.sql is not a CREATE TABLE")),
    }
}

impl Database {
    pub fn open(dir: impl Into<PathBuf>) -> Database {
        Database {
            dir: dir.into(),
            stats: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Merged statistics of the active parts of `table` (§11.5); `None` if they cannot be
    /// read. A part that is opened here for the first time and fails its checks is detached
    /// and reported on stderr (§6.4), and the query never counts it.
    pub fn table_stats(&self, table: &Table) -> Option<Arc<TableStats>> {
        let parts = active_parts(&list_parts(table.dir()).ok()?);
        let key = table.name().to_ascii_lowercase();
        let mut cache = self.stats.lock().ok()?;
        if let Some((cached_parts, stats)) = cache.get(&key)
            && *cached_parts == parts
        {
            return Some(stats.clone());
        }
        let snapshot = Snapshot::take(table, Some(&[])).ok()?;
        for detached in snapshot.detached() {
            eprintln!("{}", detached.message());
        }
        let stats = Arc::new(merge_part_stats(table, snapshot.parts()).ok()?);
        let parts = snapshot.parts().iter().map(PartReader::name).collect();
        cache.insert(key, (parts, stats.clone()));
        Some(stats)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Opens table `name`; `User` error if it does not exist.
    pub fn open_table(&self, name: &str) -> Result<Table> {
        Table::open(&self.dir, name, parse_ddl)
    }

    /// Names of the tables, sorted. A directory without `table.sql` is not a table.
    pub fn table_names(&self) -> Result<Vec<String>> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(VetaError::Io(format!("{}: {e}", self.dir.display()))),
        };
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with('.') && entry.path().join(TABLE_SQL).is_file() {
                names.push(name);
            }
        }
        names.sort();
        Ok(names)
    }

    /// `CREATE [OR REPLACE] TABLE` (contract §10.1): `OR REPLACE` drops the table if it
    /// exists; without it, an existing table is a `User` error.
    pub fn create_table(&self, create: &CreateTable) -> Result<Table> {
        let def = table_def(create)?;
        def.validate()?;
        if table_exists(&self.dir, &create.name) {
            if !create.or_replace {
                return Err(VetaError::user(format!(
                    "table {} already exists",
                    create.name
                )));
            }
            Table::drop_table(&self.dir, &create.name)?;
        }
        Table::create(&self.dir, def)
    }
}

impl Catalog for Database {
    fn table(&self, name: &str) -> Option<CatalogTable> {
        let table = self.open_table(name).ok()?;
        Some(CatalogTable {
            name: table.name().to_ascii_lowercase(),
            schema: table.schema().clone(),
            primary_key: table.primary_key().to_vec(),
            stats: self.table_stats(&table).map(|s| (*s).clone()),
        })
    }

    fn parquet_schema(&self, path: &str) -> Result<Schema> {
        Ok(ParquetFile::open(path)?.schema().clone())
    }

    fn parquet_rows(&self, path: &str) -> Option<u64> {
        ParquetFile::open(path).ok().map(|f| f.num_rows())
    }

    /// `matches / sample rows` over the union of the part samples, or `0.5 / sample rows`
    /// when no sample row matches (§11.5).
    fn selectivity(&self, table: &str, predicate: &BoundExpr) -> Option<f64> {
        let table = self.open_table(table).ok()?;
        let stats = self.table_stats(&table)?;
        let sample = &stats.sample;
        if sample.len == 0 {
            return None;
        }
        let matches = eval_predicate(predicate, sample, None).ok()?.len();
        let rows = sample.len as f64;
        Some(if matches == 0 {
            0.5 / rows
        } else {
            matches as f64 / rows
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use veta_core::DataType;

    fn temp_db(tag: &str) -> (Database, PathBuf) {
        let dir = std::env::temp_dir().join(format!("veta-db-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        (Database::open(&dir), dir)
    }

    fn create(sql: &str) -> CreateTable {
        match parse_statement(sql).unwrap() {
            Statement::CreateTable(c) => c,
            other => panic!("not a CREATE TABLE: {other:?}"),
        }
    }

    #[test]
    fn create_reopen_replace_and_catalog() {
        let (db, dir) = temp_db("create");
        assert!(db.table("Hits").is_none());
        assert_eq!(db.table_names().unwrap(), Vec::<String>::new());
        let sql = "CREATE TABLE Hits (b VARCHAR, a BIGINT NOT NULL, PRIMARY KEY (A, b)) \
                   ENGINE = MergeTree";
        db.create_table(&create(sql)).unwrap();
        assert_eq!(db.table_names().unwrap(), vec!["hits".to_string()]);

        let table = db.open_table("HITS").unwrap();
        assert_eq!(table.primary_key(), &[1, 0]);
        assert_eq!(table.schema().field(1).data_type, DataType::Int64);
        assert!(!table.schema().field(1).nullable);
        assert!(table.schema().field(0).nullable);

        let seen = db.table("hits").unwrap();
        assert_eq!(seen.name, "hits");
        assert_eq!(seen.primary_key, vec![1, 0]);

        let err = db.create_table(&create(sql)).unwrap_err();
        assert_eq!(err.exit_code(), 1);
        db.create_table(&create("CREATE OR REPLACE TABLE hits (x INTEGER)"))
            .unwrap();
        assert_eq!(db.open_table("hits").unwrap().schema().len(), 1);
        fs::remove_dir_all(&dir).unwrap();
    }

    fn int_column(index: usize, name: &str) -> BoundExpr {
        BoundExpr::column(index, name, DataType::Int64, false)
    }

    fn less_than(column: BoundExpr, bound: i64) -> BoundExpr {
        BoundExpr::new(
            veta_sql::BoundExprKind::Binary {
                op: veta_sql::expr::BinaryOp::Lt,
                left: Box::new(column),
                right: Box::new(BoundExpr::literal(veta_core::Value::Int64(bound))),
            },
            DataType::Boolean,
            false,
        )
    }

    #[test]
    fn selectivity_is_measured_on_the_union_of_the_samples() {
        use veta_core::{Chunk, Value};
        let (db, dir) = temp_db("selectivity");
        let table = db
            .create_table(&create(
                "CREATE TABLE t (a BIGINT NOT NULL, b BIGINT, PRIMARY KEY (a))",
            ))
            .unwrap();
        assert_eq!(
            db.selectivity("t", &less_than(int_column(0, "t.a"), 10)),
            None
        );
        // Two parts of 3000 rows: a in 0..3000 and 3000..6000, b = a % 10.
        for part in 0..2i64 {
            let rows: Vec<Vec<Value>> = (part * 3000..(part + 1) * 3000)
                .map(|a| vec![Value::Int64(a), Value::Int64(a % 10)])
                .collect();
            let chunk = Chunk::from_rows(&table.schema().data_types(), &rows).unwrap();
            veta_storage::insert(&table, &chunk).unwrap();
        }
        let seen = db.table("t").unwrap();
        let stats = seen.stats.expect("stats are loaded");
        assert_eq!(stats.rows, 6000);
        assert_eq!(stats.sample.len, 2048);
        // Sample positions floor(i · 3000 / 1024) < 1500 for i < 512: 512 of 2048 rows.
        let sel = db.selectivity("t", &less_than(int_column(0, "t.a"), 1500));
        assert_eq!(sel, Some(0.25));
        let sel = db.selectivity("t", &less_than(int_column(0, "t.a"), 6000));
        assert_eq!(sel, Some(1.0));
        // No sample row matches: 0.5 / sample rows.
        let sel = db.selectivity("t", &less_than(int_column(0, "t.a"), 0));
        assert_eq!(sel, Some(0.5 / 2048.0));
        assert_eq!(
            db.selectivity("missing", &less_than(int_column(0, "x"), 0)),
            None
        );
        assert_eq!(
            db.parquet_rows(&dir.join("none.parquet").to_string_lossy()),
            None
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unknown_primary_key_column_is_a_user_error() {
        let bad = create("CREATE TABLE t (a INTEGER, PRIMARY KEY (b))");
        assert_eq!(table_def(&bad).unwrap_err().exit_code(), 1);
    }
}
