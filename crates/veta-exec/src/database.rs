//! A database: the `--db` directory with one subdirectory per table (contract §5.1).
//!
//! `table.sql` holds the canonical `CREATE TABLE`, which is parsed back into the
//! [`TableDef`] of `veta-storage` whenever a table is opened. [`Database`] is also the
//! catalog the binder resolves names against.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use veta_core::{Field, Result, Schema, VetaError};
use veta_parquet::ParquetFile;
use veta_sql::ast::CreateTable;
use veta_sql::{BoundExpr, Catalog, CatalogTable, Statement, parse_statement};
use veta_storage::table::{TABLE_SQL, table_exists};
use veta_storage::{Table, TableDef};

/// The directory of `--db`; it may not exist until the first `CREATE TABLE`.
#[derive(Debug, Clone)]
pub struct Database {
    dir: PathBuf,
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
        Database { dir: dir.into() }
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
            stats: None,
        })
    }

    fn parquet_schema(&self, path: &str) -> Result<Schema> {
        Ok(ParquetFile::open(path)?.schema().clone())
    }

    fn selectivity(&self, _table: &str, _predicate: &BoundExpr) -> Option<f64> {
        None
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

    #[test]
    fn unknown_primary_key_column_is_a_user_error() {
        let bad = create("CREATE TABLE t (a INTEGER, PRIMARY KEY (b))");
        assert_eq!(table_def(&bad).unwrap_err().exit_code(), 1);
    }
}
