//! Tables (contract §5.1): a directory `DIR/<table>/` (name in lowercase) with the
//! canonical `CREATE TABLE` in `table.sql`, the block-number watermark, the lock files and
//! the parts.
//!
//! This crate does not parse SQL: whoever parses the statement builds the [`TableDef`]
//! (with its canonical DDL) and [`Table::open`] gets a parser for `table.sql` from the
//! caller. Creation is atomic: the directory is assembled under a hidden name in the
//! database directory and renamed into place.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use veta_core::{Result, Schema, VetaError};

use crate::fsutil::{io_err, remove_dir_all_if_exists, sync_dir, write_synced};

pub const TABLE_SQL: &str = "table.sql";
pub const BLOCK_NUMBER_TXT: &str = "block_number.txt";
pub const INSERT_LOCK: &str = ".insert.lock";
pub const MERGE_LOCK: &str = ".merge.lock";
pub const READERS_LOCK: &str = ".readers.lock";
pub const DETACHED_DIR: &str = "detached";
/// Prefix of every part under construction (`tmp_insert_<b>`, `tmp_merge_<name>`).
pub const TMP_PREFIX: &str = "tmp_";
pub const TMP_INSERT_PREFIX: &str = "tmp_insert_";
/// Prefix of a part being deleted.
pub const DELETE_TMP_PREFIX: &str = "delete_tmp_";

/// Definition of a table: what `CREATE TABLE` declared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableDef {
    pub name: String,
    pub schema: Schema,
    /// Schema indices of the primary-key columns, in key order (empty: insertion order).
    pub primary_key: Vec<usize>,
    /// Canonical `CREATE TABLE`, written to `table.sql`.
    pub ddl: String,
}

/// Whether `name` can be a table directory name (after lowercasing).
fn valid_table_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && !name
            .chars()
            .any(|c| c == '/' || c == '\0' || c.is_control())
}

impl TableDef {
    /// Checks what the storage relies on: a usable name, at least one column, declarable
    /// types, column names unique and without tabs or line breaks (they go into
    /// `part.txt`), and key columns in range and distinct.
    pub fn validate(&self) -> Result<()> {
        if !valid_table_name(&self.name) {
            return Err(VetaError::user(format!(
                "invalid table name {:?}",
                self.name
            )));
        }
        if self.schema.is_empty() {
            return Err(VetaError::user(format!(
                "table {} has no columns",
                self.name
            )));
        }
        let mut names = HashSet::new();
        for field in &self.schema.fields {
            if field.name.is_empty() || field.name.contains(['\t', '\n', '\r']) {
                return Err(VetaError::user(format!(
                    "invalid column name {:?}",
                    field.name
                )));
            }
            if !names.insert(field.name.to_ascii_lowercase()) {
                return Err(VetaError::user(format!(
                    "duplicate column name {}",
                    field.name
                )));
            }
            if !field.data_type.is_declarable() {
                return Err(VetaError::user(format!(
                    "type {} cannot be declared in a table",
                    field.data_type
                )));
            }
        }
        let mut seen = HashSet::new();
        for &c in &self.primary_key {
            if c >= self.schema.len() {
                return Err(VetaError::internal(format!(
                    "primary key column {c} out of range"
                )));
            }
            if !seen.insert(c) {
                return Err(VetaError::user(format!(
                    "column {} appears twice in the primary key",
                    self.schema.field(c).name
                )));
            }
        }
        Ok(())
    }

    /// Directory name of the table: its name in lowercase.
    pub fn dir_name(&self) -> String {
        self.name.to_lowercase()
    }
}

/// Directory of table `name` in database `db`.
pub fn table_dir(db: &Path, name: &str) -> PathBuf {
    db.join(name.to_lowercase())
}

/// Whether table `name` exists in `db`.
pub fn table_exists(db: &Path, name: &str) -> bool {
    table_dir(db, name).join(TABLE_SQL).is_file()
}

/// Reads `table.sql` of table `name`; `User` error if the table does not exist.
pub fn read_ddl(db: &Path, name: &str) -> Result<String> {
    let path = table_dir(db, name).join(TABLE_SQL);
    match fs::read(&path) {
        Ok(bytes) => String::from_utf8(bytes)
            .map_err(|_| VetaError::Io(format!("{}: not UTF-8", path.display()))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Err(VetaError::user(format!("table {name} does not exist")))
        }
        Err(e) => Err(io_err(&path, e)),
    }
}

/// An existing table.
#[derive(Debug, Clone)]
pub struct Table {
    def: TableDef,
    dir: PathBuf,
}

impl Table {
    /// Creates the table directory with `table.sql`, `block_number.txt` (0) and the lock
    /// files, atomically. `User` error if the table already exists.
    pub fn create(db: &Path, def: TableDef) -> Result<Table> {
        def.validate()?;
        fs::create_dir_all(db).map_err(|e| io_err(db, e))?;
        let dir = db.join(def.dir_name());
        if dir.exists() {
            return Err(VetaError::user(format!(
                "table {} already exists",
                def.name
            )));
        }
        let tmp = db.join(format!(".create_{}_{}", def.dir_name(), std::process::id()));
        remove_dir_all_if_exists(&tmp)?;
        fs::create_dir(&tmp).map_err(|e| io_err(&tmp, e))?;
        let built = (|| -> Result<()> {
            let mut ddl = def.ddl.clone();
            if !ddl.ends_with('\n') {
                ddl.push('\n');
            }
            write_synced(&tmp.join(TABLE_SQL), ddl.as_bytes())?;
            write_synced(&tmp.join(BLOCK_NUMBER_TXT), b"0\n")?;
            for lock in [INSERT_LOCK, MERGE_LOCK, READERS_LOCK] {
                write_synced(&tmp.join(lock), b"")?;
            }
            sync_dir(&tmp)?;
            if dir.exists() {
                return Err(VetaError::user(format!(
                    "table {} already exists",
                    def.name
                )));
            }
            fs::rename(&tmp, &dir).map_err(|e| io_err(&dir, e))?;
            sync_dir(db)
        })();
        if let Err(err) = built {
            let _ = remove_dir_all_if_exists(&tmp);
            return Err(err);
        }
        Ok(Table { def, dir })
    }

    /// Opens table `name`: reads `table.sql` and builds its definition with `parse` (the
    /// SQL parser lives outside this crate).
    pub fn open(
        db: &Path,
        name: &str,
        parse: impl FnOnce(&str) -> Result<TableDef>,
    ) -> Result<Table> {
        let ddl = read_ddl(db, name)?;
        let def = parse(ddl.trim_end())?;
        def.validate()?;
        if def.dir_name() != name.to_lowercase() {
            return Err(VetaError::internal(format!(
                "table.sql of {name} defines table {}",
                def.name
            )));
        }
        Ok(Table {
            def,
            dir: table_dir(db, name),
        })
    }

    /// A table from a definition already read, without touching the disk.
    pub fn from_def(db: &Path, def: TableDef) -> Table {
        let dir = db.join(def.dir_name());
        Table { def, dir }
    }

    /// Deletes table `name` (for `DROP` and `CREATE OR REPLACE`): renames its directory to a
    /// hidden name, so it vanishes at once, and removes it. `false` if it did not exist.
    pub fn drop_table(db: &Path, name: &str) -> Result<bool> {
        let dir = table_dir(db, name);
        if !dir.exists() {
            return Ok(false);
        }
        let doomed = db.join(format!(
            ".drop_{}_{}",
            name.to_lowercase(),
            std::process::id()
        ));
        remove_dir_all_if_exists(&doomed)?;
        match fs::rename(&dir, &doomed) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(io_err(&dir, e)),
        }
        sync_dir(db)?;
        remove_dir_all_if_exists(&doomed)?;
        Ok(true)
    }

    pub fn def(&self) -> &TableDef {
        &self.def
    }

    pub fn name(&self) -> &str {
        &self.def.name
    }

    pub fn schema(&self) -> &Schema {
        &self.def.schema
    }

    pub fn primary_key(&self) -> &[usize] {
        &self.def.primary_key
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn lock_path(&self, lock: &str) -> PathBuf {
        self.dir.join(lock)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::fsutil::testdir::TestDir;
    use veta_core::{DataType, Field};

    pub(crate) fn sample_def(name: &str) -> TableDef {
        TableDef {
            name: name.to_string(),
            schema: Schema::new(vec![
                Field::new("a", DataType::Int64, false),
                Field::new("b", DataType::Varchar, true),
                Field::new("c", DataType::Float64, true),
            ]),
            primary_key: vec![1, 0],
            ddl: format!(
                "CREATE TABLE {name} (a BIGINT NOT NULL, b VARCHAR, c DOUBLE, PRIMARY KEY (b, a))"
            ),
        }
    }

    #[test]
    fn create_open_and_drop() {
        let db = TestDir::new("table");
        let def = sample_def("Hits");
        let table = Table::create(db.path(), def.clone()).unwrap();
        assert_eq!(table.dir(), db.path().join("hits"));
        for file in [
            TABLE_SQL,
            BLOCK_NUMBER_TXT,
            INSERT_LOCK,
            MERGE_LOCK,
            READERS_LOCK,
        ] {
            assert!(table.dir().join(file).is_file(), "{file}");
        }
        assert_eq!(
            fs::read_to_string(table.dir().join(BLOCK_NUMBER_TXT)).unwrap(),
            "0\n"
        );
        assert!(table_exists(db.path(), "HITS"));
        let err = Table::create(db.path(), def.clone()).unwrap_err();
        assert_eq!(err.exit_code(), 1);

        let mut seen = String::new();
        let opened = Table::open(db.path(), "hits", |ddl| {
            seen = ddl.to_string();
            Ok(def.clone())
        })
        .unwrap();
        assert_eq!(seen, def.ddl);
        assert_eq!(opened.def(), &def);
        assert_eq!(opened.primary_key(), &[1, 0]);

        let wrong = Table::open(db.path(), "hits", |_| Ok(sample_def("other")));
        assert!(wrong.is_err());

        assert!(Table::drop_table(db.path(), "hits").unwrap());
        assert!(!table_exists(db.path(), "hits"));
        assert!(!Table::drop_table(db.path(), "hits").unwrap());
        let missing = Table::open(db.path(), "hits", |_| Ok(def.clone())).unwrap_err();
        assert_eq!(missing.exit_code(), 1);
        let leftovers: Vec<_> = fs::read_dir(db.path()).unwrap().collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn invalid_definitions_are_rejected() {
        let mut bad_name = sample_def("x");
        bad_name.name = "../x".into();
        assert!(bad_name.validate().is_err());
        let mut dup = sample_def("x");
        dup.schema.fields[1].name = "A".into();
        assert!(dup.validate().is_err());
        let mut tab = sample_def("x");
        tab.schema.fields[0].name = "a\tb".into();
        assert!(tab.validate().is_err());
        let mut key = sample_def("x");
        key.primary_key = vec![0, 0];
        assert!(key.validate().is_err());
        key.primary_key = vec![3];
        assert!(key.validate().is_err());
        let mut wide = sample_def("x");
        wide.schema.fields[0].data_type = DataType::Int128;
        assert!(wide.validate().is_err());
        assert!(sample_def("x").validate().is_ok());
    }
}
