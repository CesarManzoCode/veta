//! What the binder needs to know about tables and Parquet files (contract §10.2, §11.5).
//!
//! The storage layer implements [`Catalog`] in the CLI; the binder only sees schemas,
//! primary keys and, from K2 on, statistics and predicate selectivities.

use veta_core::stats::TableStats;
use veta_core::{Result, Schema, VetaError};

use crate::expr::BoundExpr;

/// A stored table as the binder sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogTable {
    /// Lower-case table name (contract §5.1).
    pub name: String,
    pub schema: Schema,
    /// Indices into `schema` of the primary-key columns, in key order.
    pub primary_key: Vec<usize>,
    /// Merged statistics of the active parts; `None` when they were not loaded.
    pub stats: Option<TableStats>,
}

pub trait Catalog {
    /// The table called `name` (matched case-insensitively), if it exists.
    fn table(&self, name: &str) -> Option<CatalogTable>;

    /// Schema of the Parquet file at `path`, for `read_parquet('path')`.
    fn parquet_schema(&self, path: &str) -> Result<Schema>;

    /// Selectivity of a single-table predicate measured on the table sample (§11.5).
    /// `None` means unknown; it stays unknown until K2.
    fn selectivity(&self, table: &str, predicate: &BoundExpr) -> Option<f64>;
}

/// In-memory catalog for tests and for callers that build schemas by hand.
#[derive(Debug, Clone, Default)]
pub struct MemoryCatalog {
    pub tables: Vec<CatalogTable>,
    pub parquet_files: Vec<(String, Schema)>,
}

impl MemoryCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_table(&mut self, name: &str, schema: Schema, primary_key: Vec<usize>) {
        self.tables.push(CatalogTable {
            name: name.to_ascii_lowercase(),
            schema,
            primary_key,
            stats: None,
        });
    }

    pub fn add_parquet(&mut self, path: &str, schema: Schema) {
        self.parquet_files.push((path.to_string(), schema));
    }
}

impl Catalog for MemoryCatalog {
    fn table(&self, name: &str) -> Option<CatalogTable> {
        self.tables
            .iter()
            .find(|t| t.name.eq_ignore_ascii_case(name))
            .cloned()
    }

    fn parquet_schema(&self, path: &str) -> Result<Schema> {
        self.parquet_files
            .iter()
            .find(|(p, _)| p == path)
            .map(|(_, s)| s.clone())
            .ok_or_else(|| VetaError::Io(format!("{path}: no such file")))
    }

    fn selectivity(&self, _table: &str, _predicate: &BoundExpr) -> Option<f64> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use veta_core::{DataType, Field};

    #[test]
    fn lookup_ignores_case_and_selectivity_is_unknown() {
        let mut catalog = MemoryCatalog::new();
        let schema = Schema::new(vec![Field::new("a", DataType::Int64, false)]);
        catalog.add_table("Hits", schema.clone(), vec![0]);
        let table = catalog.table("HITS").unwrap();
        assert_eq!(table.name, "hits");
        assert_eq!(table.primary_key, vec![0]);
        assert!(catalog.table("other").is_none());
        let predicate = BoundExpr::literal(veta_core::Value::Boolean(true));
        assert_eq!(catalog.selectivity("hits", &predicate), None);
    }

    #[test]
    fn parquet_schema_lookup() {
        let mut catalog = MemoryCatalog::new();
        let schema = Schema::new(vec![Field::new("x", DataType::Varchar, true)]);
        catalog.add_parquet("/data/x.parquet", schema.clone());
        assert_eq!(catalog.parquet_schema("/data/x.parquet").unwrap(), schema);
        assert_eq!(
            catalog
                .parquet_schema("/data/y.parquet")
                .unwrap_err()
                .exit_code(),
            2
        );
    }
}
