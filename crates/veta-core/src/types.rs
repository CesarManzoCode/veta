//! Data types, fields and schemas (contract §4).

use std::fmt;

use crate::error::{Result, VetaError};

/// Maximum decimal precision (contract §4).
pub const MAX_DECIMAL_PRECISION: u8 = 38;
/// Decimals up to this precision are stored in `i64`; wider ones in `i128` (contract §4).
pub const MAX_DECIMAL64_PRECISION: u8 = 18;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataType {
    Boolean,
    Int16,
    Int32,
    Int64,
    /// Result-only type (`SUM(Int64)`); it cannot be declared in a table.
    Int128,
    Float64,
    Decimal {
        precision: u8,
        scale: u8,
    },
    Varchar,
    /// Days since 1970-01-01.
    Date,
    /// Seconds since 1970-01-01 00:00:00 UTC.
    Timestamp,
}

impl DataType {
    /// Canonical SQL name, the one written to `table.sql` and `part.txt`.
    pub fn sql_name(&self) -> String {
        match self {
            DataType::Boolean => "BOOLEAN".into(),
            DataType::Int16 => "SMALLINT".into(),
            DataType::Int32 => "INTEGER".into(),
            DataType::Int64 => "BIGINT".into(),
            DataType::Int128 => "INT128".into(),
            DataType::Float64 => "DOUBLE".into(),
            DataType::Decimal { precision, scale } => format!("DECIMAL({precision},{scale})"),
            DataType::Varchar => "VARCHAR".into(),
            DataType::Date => "DATE".into(),
            DataType::Timestamp => "TIMESTAMP".into(),
        }
    }

    /// Parses a declarable SQL type name of §4, case-insensitively.
    ///
    /// `INT128` is result-only and is rejected; `VARCHAR(n)`/`CHAR(n)` ignore `n`;
    /// `DECIMAL(p)` means scale 0.
    pub fn parse(name: &str) -> Result<DataType> {
        let normalized = name.split_whitespace().collect::<Vec<_>>().join(" ");
        let upper = normalized.to_ascii_uppercase();
        let (base, args) = match upper.find('(') {
            Some(open) => {
                let close = upper
                    .rfind(')')
                    .filter(|&c| c > open && upper[c + 1..].trim().is_empty())
                    .ok_or_else(|| VetaError::user(format!("invalid type: {name}")))?;
                (upper[..open].trim(), Some(&upper[open + 1..close]))
            }
            None => (upper.trim(), None),
        };
        let no_args = |dt: DataType| -> Result<DataType> {
            match args {
                None => Ok(dt),
                Some(_) => Err(VetaError::user(format!("invalid type: {name}"))),
            }
        };
        match base {
            "BOOLEAN" | "BOOL" => no_args(DataType::Boolean),
            "SMALLINT" | "INT2" => no_args(DataType::Int16),
            "INTEGER" | "INT" | "INT4" => no_args(DataType::Int32),
            "BIGINT" | "INT8" => no_args(DataType::Int64),
            "DOUBLE" | "FLOAT8" | "DOUBLE PRECISION" | "FLOAT" | "REAL" => {
                no_args(DataType::Float64)
            }
            "VARCHAR" | "CHAR" => {
                if let Some(a) = args {
                    parse_u32_arg(a.trim(), name)?;
                }
                Ok(DataType::Varchar)
            }
            "TEXT" | "STRING" => no_args(DataType::Varchar),
            "DATE" => no_args(DataType::Date),
            "TIMESTAMP" | "DATETIME" => no_args(DataType::Timestamp),
            "DECIMAL" | "NUMERIC" => {
                let a = args
                    .ok_or_else(|| VetaError::unsupported(format!("{base} without precision")))?;
                let parts: Vec<&str> = a.split(',').map(str::trim).collect();
                let (p, s) = match parts.as_slice() {
                    [p] => (parse_u32_arg(p, name)?, 0),
                    [p, s] => (parse_u32_arg(p, name)?, parse_u32_arg(s, name)?),
                    _ => return Err(VetaError::user(format!("invalid type: {name}"))),
                };
                DataType::decimal(p, s)
            }
            _ => Err(VetaError::unsupported(format!("type {name}"))),
        }
    }

    /// Validated `Decimal { precision, scale }`: `1 ≤ precision ≤ 38`, `scale ≤ precision`.
    pub fn decimal(precision: u32, scale: u32) -> Result<DataType> {
        if precision == 0 || precision > MAX_DECIMAL_PRECISION as u32 || scale > precision {
            return Err(VetaError::user(format!(
                "invalid decimal type DECIMAL({precision},{scale})"
            )));
        }
        Ok(DataType::Decimal {
            precision: precision as u8,
            scale: scale as u8,
        })
    }

    pub fn is_integer(&self) -> bool {
        matches!(
            self,
            DataType::Int16 | DataType::Int32 | DataType::Int64 | DataType::Int128
        )
    }

    pub fn is_numeric(&self) -> bool {
        self.is_integer() || matches!(self, DataType::Float64 | DataType::Decimal { .. })
    }

    /// Whether a column of this type may appear in `CREATE TABLE` (everything but `Int128`).
    pub fn is_declarable(&self) -> bool {
        !matches!(self, DataType::Int128)
    }

    /// Bytes per value of the physical representation, `None` for `Varchar`.
    pub fn fixed_width(&self) -> Option<usize> {
        match self {
            DataType::Boolean => Some(1),
            DataType::Int16 => Some(2),
            DataType::Int32 | DataType::Date => Some(4),
            DataType::Int64 | DataType::Float64 | DataType::Timestamp => Some(8),
            DataType::Int128 => Some(16),
            DataType::Decimal { precision, .. } => {
                if *precision <= MAX_DECIMAL64_PRECISION {
                    Some(8)
                } else {
                    Some(16)
                }
            }
            DataType::Varchar => None,
        }
    }
}

fn parse_u32_arg(text: &str, name: &str) -> Result<u32> {
    text.parse::<u32>()
        .map_err(|_| VetaError::user(format!("invalid type: {name}")))
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.sql_name())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
}

impl Field {
    pub fn new(name: impl Into<String>, data_type: DataType, nullable: bool) -> Self {
        Field {
            name: name.into(),
            data_type,
            nullable,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Schema {
    pub fields: Vec<Field>,
}

impl Schema {
    pub fn new(fields: Vec<Field>) -> Self {
        Schema { fields }
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    pub fn field(&self, index: usize) -> &Field {
        &self.fields[index]
    }

    /// Index of the column with exactly this name.
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| f.name == name)
    }

    /// Index of the column whose name matches ignoring ASCII case (Parquet loading, §13).
    pub fn index_of_ignore_case(&self, name: &str) -> Option<usize> {
        self.fields
            .iter()
            .position(|f| f.name.eq_ignore_ascii_case(name))
    }

    pub fn data_types(&self) -> Vec<DataType> {
        self.fields.iter().map(|f| f.data_type).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_name_of_the_contract() {
        let cases = [
            ("BOOLEAN", DataType::Boolean),
            ("smallint", DataType::Int16),
            ("INT2", DataType::Int16),
            ("INTEGER", DataType::Int32),
            ("Int", DataType::Int32),
            ("INT4", DataType::Int32),
            ("BIGINT", DataType::Int64),
            ("INT8", DataType::Int64),
            ("DOUBLE", DataType::Float64),
            ("FLOAT8", DataType::Float64),
            ("double   precision", DataType::Float64),
            ("FLOAT", DataType::Float64),
            ("REAL", DataType::Float64),
            ("VARCHAR", DataType::Varchar),
            ("VARCHAR(25)", DataType::Varchar),
            ("TEXT", DataType::Varchar),
            ("CHAR(1)", DataType::Varchar),
            ("CHAR", DataType::Varchar),
            ("STRING", DataType::Varchar),
            ("DATE", DataType::Date),
            ("TIMESTAMP", DataType::Timestamp),
            ("DATETIME", DataType::Timestamp),
        ];
        for (name, dt) in cases {
            assert_eq!(DataType::parse(name).unwrap(), dt, "{name}");
        }
        assert_eq!(
            DataType::parse("DECIMAL(15,2)").unwrap(),
            DataType::Decimal {
                precision: 15,
                scale: 2
            }
        );
        assert_eq!(
            DataType::parse("numeric( 38 , 10 )").unwrap(),
            DataType::Decimal {
                precision: 38,
                scale: 10
            }
        );
        assert_eq!(
            DataType::parse("DECIMAL(9)").unwrap(),
            DataType::Decimal {
                precision: 9,
                scale: 0
            }
        );
    }

    #[test]
    fn rejects_invalid_names() {
        for name in [
            "INT128",
            "DECIMAL",
            "DECIMAL(39,2)",
            "DECIMAL(0,0)",
            "DECIMAL(5,6)",
            "DECIMAL(5,2",
            "INTEGER(3)",
            "BLOB",
            "VARCHAR(x)",
        ] {
            let err = DataType::parse(name).unwrap_err();
            assert_eq!(err.exit_code(), 1, "{name}");
        }
    }

    #[test]
    fn sql_names_reparse() {
        for dt in [
            DataType::Boolean,
            DataType::Int16,
            DataType::Int32,
            DataType::Int64,
            DataType::Float64,
            DataType::Decimal {
                precision: 20,
                scale: 4,
            },
            DataType::Varchar,
            DataType::Date,
            DataType::Timestamp,
        ] {
            assert_eq!(DataType::parse(&dt.sql_name()).unwrap(), dt);
        }
        assert_eq!(DataType::Int128.sql_name(), "INT128");
        assert!(!DataType::Int128.is_declarable());
    }

    #[test]
    fn decimal_width_follows_precision() {
        assert_eq!(DataType::decimal(18, 2).unwrap().fixed_width(), Some(8));
        assert_eq!(DataType::decimal(19, 2).unwrap().fixed_width(), Some(16));
        assert_eq!(DataType::Varchar.fixed_width(), None);
    }

    #[test]
    fn schema_lookup() {
        let schema = Schema::new(vec![
            Field::new("UserID", DataType::Int64, false),
            Field::new("URL", DataType::Varchar, true),
        ]);
        assert_eq!(schema.len(), 2);
        assert_eq!(schema.index_of("URL"), Some(1));
        assert_eq!(schema.index_of("url"), None);
        assert_eq!(schema.index_of_ignore_case("userid"), Some(0));
        assert_eq!(
            schema.data_types(),
            vec![DataType::Int64, DataType::Varchar]
        );
    }
}
