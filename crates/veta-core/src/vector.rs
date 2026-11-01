//! Typed column vectors and chunks (contract §12).
//!
//! A [`Vector`] is a typed buffer plus an optional validity bitmap; `None` means every row
//! is valid. A NULL slot keeps a default value in the buffer (zero, or an empty string).

use crate::bitmap::Bitmap;
use crate::decimal;
use crate::error::{Result, VetaError};
use crate::types::{DataType, MAX_DECIMAL64_PRECISION};
use crate::value::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum VectorData {
    Boolean(Vec<bool>),
    Int16(Vec<i16>),
    Int32(Vec<i32>),
    Int64(Vec<i64>),
    Int128(Vec<i128>),
    Float64(Vec<f64>),
    /// `Decimal` with precision ≤ 18.
    Decimal64 {
        values: Vec<i64>,
        precision: u8,
        scale: u8,
    },
    /// `Decimal` with 18 < precision ≤ 38.
    Decimal128 {
        values: Vec<i128>,
        precision: u8,
        scale: u8,
    },
    /// String `i` is `bytes[offsets[i]..offsets[i + 1]]`; `offsets` has `len + 1` entries.
    Varchar {
        offsets: Vec<u32>,
        bytes: Vec<u8>,
    },
    /// Days since 1970-01-01.
    Date(Vec<i32>),
    /// Seconds since 1970-01-01 00:00:00 UTC.
    Timestamp(Vec<i64>),
}

/// Applies `$fixed` to the value buffer of every fixed-width variant (rebuilding the same
/// variant) and `$var` to the offsets and bytes of `Varchar`.
macro_rules! map_data {
    ($data:expr, |$v:ident| $fixed:expr, |$o:ident, $b:ident| $var:expr) => {
        match $data {
            VectorData::Boolean($v) => VectorData::Boolean($fixed),
            VectorData::Int16($v) => VectorData::Int16($fixed),
            VectorData::Int32($v) => VectorData::Int32($fixed),
            VectorData::Int64($v) => VectorData::Int64($fixed),
            VectorData::Int128($v) => VectorData::Int128($fixed),
            VectorData::Float64($v) => VectorData::Float64($fixed),
            VectorData::Decimal64 {
                values: $v,
                precision,
                scale,
            } => VectorData::Decimal64 {
                values: $fixed,
                precision: *precision,
                scale: *scale,
            },
            VectorData::Decimal128 {
                values: $v,
                precision,
                scale,
            } => VectorData::Decimal128 {
                values: $fixed,
                precision: *precision,
                scale: *scale,
            },
            VectorData::Date($v) => VectorData::Date($fixed),
            VectorData::Timestamp($v) => VectorData::Timestamp($fixed),
            VectorData::Varchar {
                offsets: $o,
                bytes: $b,
            } => $var,
        }
    };
}

fn type_mismatch(value: &Value, data_type: DataType) -> VetaError {
    VetaError::internal(format!("value {value:?} does not fit a {data_type} vector"))
}

fn checked_offset(len: usize) -> Result<u32> {
    u32::try_from(len).map_err(|_| VetaError::internal("string vector exceeds 4 GiB"))
}

impl VectorData {
    pub fn with_capacity(data_type: DataType, capacity: usize) -> VectorData {
        match data_type {
            DataType::Boolean => VectorData::Boolean(Vec::with_capacity(capacity)),
            DataType::Int16 => VectorData::Int16(Vec::with_capacity(capacity)),
            DataType::Int32 => VectorData::Int32(Vec::with_capacity(capacity)),
            DataType::Int64 => VectorData::Int64(Vec::with_capacity(capacity)),
            DataType::Int128 => VectorData::Int128(Vec::with_capacity(capacity)),
            DataType::Float64 => VectorData::Float64(Vec::with_capacity(capacity)),
            DataType::Decimal { precision, scale } => {
                if precision <= MAX_DECIMAL64_PRECISION {
                    VectorData::Decimal64 {
                        values: Vec::with_capacity(capacity),
                        precision,
                        scale,
                    }
                } else {
                    VectorData::Decimal128 {
                        values: Vec::with_capacity(capacity),
                        precision,
                        scale,
                    }
                }
            }
            DataType::Varchar => {
                let mut offsets = Vec::with_capacity(capacity + 1);
                offsets.push(0);
                VectorData::Varchar {
                    offsets,
                    bytes: Vec::new(),
                }
            }
            DataType::Date => VectorData::Date(Vec::with_capacity(capacity)),
            DataType::Timestamp => VectorData::Timestamp(Vec::with_capacity(capacity)),
        }
    }

    pub fn data_type(&self) -> DataType {
        match self {
            VectorData::Boolean(_) => DataType::Boolean,
            VectorData::Int16(_) => DataType::Int16,
            VectorData::Int32(_) => DataType::Int32,
            VectorData::Int64(_) => DataType::Int64,
            VectorData::Int128(_) => DataType::Int128,
            VectorData::Float64(_) => DataType::Float64,
            VectorData::Decimal64 {
                precision, scale, ..
            }
            | VectorData::Decimal128 {
                precision, scale, ..
            } => DataType::Decimal {
                precision: *precision,
                scale: *scale,
            },
            VectorData::Varchar { .. } => DataType::Varchar,
            VectorData::Date(_) => DataType::Date,
            VectorData::Timestamp(_) => DataType::Timestamp,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            VectorData::Boolean(v) => v.len(),
            VectorData::Int16(v) => v.len(),
            VectorData::Int32(v) | VectorData::Date(v) => v.len(),
            VectorData::Int64(v) | VectorData::Timestamp(v) => v.len(),
            VectorData::Int128(v) => v.len(),
            VectorData::Float64(v) => v.len(),
            VectorData::Decimal64 { values, .. } => values.len(),
            VectorData::Decimal128 { values, .. } => values.len(),
            VectorData::Varchar { offsets, .. } => offsets.len() - 1,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Bytes of string `i` of a `Varchar` vector.
    pub fn str_at(&self, i: usize) -> Option<&[u8]> {
        match self {
            VectorData::Varchar { offsets, bytes } => {
                Some(&bytes[offsets[i] as usize..offsets[i + 1] as usize])
            }
            _ => None,
        }
    }

    /// Value at row `i`, ignoring validity.
    pub fn get(&self, i: usize) -> Value {
        match self {
            VectorData::Boolean(v) => Value::Boolean(v[i]),
            VectorData::Int16(v) => Value::Int16(v[i]),
            VectorData::Int32(v) => Value::Int32(v[i]),
            VectorData::Int64(v) => Value::Int64(v[i]),
            VectorData::Int128(v) => Value::Int128(v[i]),
            VectorData::Float64(v) => Value::Float64(v[i]),
            VectorData::Decimal64 {
                values,
                precision,
                scale,
            } => Value::Decimal {
                value: values[i] as i128,
                precision: *precision,
                scale: *scale,
            },
            VectorData::Decimal128 {
                values,
                precision,
                scale,
            } => Value::Decimal {
                value: values[i],
                precision: *precision,
                scale: *scale,
            },
            VectorData::Varchar { offsets, bytes } => {
                Value::Varchar(bytes[offsets[i] as usize..offsets[i + 1] as usize].to_vec())
            }
            VectorData::Date(v) => Value::Date(v[i]),
            VectorData::Timestamp(v) => Value::Timestamp(v[i]),
        }
    }

    /// Appends the default value used for NULL slots.
    pub fn push_default(&mut self) {
        match self {
            VectorData::Boolean(v) => v.push(false),
            VectorData::Int16(v) => v.push(0),
            VectorData::Int32(v) | VectorData::Date(v) => v.push(0),
            VectorData::Int64(v) | VectorData::Timestamp(v) => v.push(0),
            VectorData::Int128(v) => v.push(0),
            VectorData::Float64(v) => v.push(0.0),
            VectorData::Decimal64 { values, .. } => values.push(0),
            VectorData::Decimal128 { values, .. } => values.push(0),
            VectorData::Varchar { offsets, .. } => {
                let last = *offsets.last().unwrap_or(&0);
                offsets.push(last);
            }
        }
    }

    /// Appends a non-NULL value of exactly this vector's type. A decimal must have this
    /// vector's scale and fit its precision.
    pub fn push_value(&mut self, value: &Value) -> Result<()> {
        let data_type = self.data_type();
        match (&mut *self, value) {
            (VectorData::Boolean(v), Value::Boolean(x)) => v.push(*x),
            (VectorData::Int16(v), Value::Int16(x)) => v.push(*x),
            (VectorData::Int32(v), Value::Int32(x)) => v.push(*x),
            (VectorData::Int64(v), Value::Int64(x)) => v.push(*x),
            (VectorData::Int128(v), Value::Int128(x)) => v.push(*x),
            (VectorData::Float64(v), Value::Float64(x)) => v.push(*x),
            (VectorData::Date(v), Value::Date(x)) => v.push(*x),
            (VectorData::Timestamp(v), Value::Timestamp(x)) => v.push(*x),
            (
                VectorData::Decimal64 {
                    values,
                    precision,
                    scale,
                },
                Value::Decimal {
                    value: x, scale: s, ..
                },
            ) if *s == *scale && decimal::fits_precision(*x, *precision) => values.push(*x as i64),
            (
                VectorData::Decimal128 {
                    values,
                    precision,
                    scale,
                },
                Value::Decimal {
                    value: x, scale: s, ..
                },
            ) if *s == *scale && decimal::fits_precision(*x, *precision) => values.push(*x),
            (VectorData::Varchar { offsets, bytes }, Value::Varchar(x)) => {
                let end = checked_offset(bytes.len() + x.len())?;
                bytes.extend_from_slice(x);
                offsets.push(end);
            }
            _ => return Err(type_mismatch(value, data_type)),
        }
        Ok(())
    }

    pub fn slice(&self, offset: usize, len: usize) -> VectorData {
        assert!(offset + len <= self.len(), "slice out of range");
        map_data!(
            self,
            |v| v[offset..offset + len].to_vec(),
            |offsets, bytes| {
                let start = offsets[offset];
                let end = offsets[offset + len];
                VectorData::Varchar {
                    offsets: offsets[offset..=offset + len]
                        .iter()
                        .map(|o| o - start)
                        .collect(),
                    bytes: bytes[start as usize..end as usize].to_vec(),
                }
            }
        )
    }

    pub fn gather(&self, indices: &[u32]) -> VectorData {
        map_data!(
            self,
            |v| indices.iter().map(|&i| v[i as usize]).collect(),
            |offsets, bytes| {
                let mut out_offsets = Vec::with_capacity(indices.len() + 1);
                let mut out_bytes = Vec::new();
                out_offsets.push(0u32);
                for &i in indices {
                    let i = i as usize;
                    out_bytes
                        .extend_from_slice(&bytes[offsets[i] as usize..offsets[i + 1] as usize]);
                    out_offsets.push(out_bytes.len() as u32);
                }
                VectorData::Varchar {
                    offsets: out_offsets,
                    bytes: out_bytes,
                }
            }
        )
    }

    /// Appends every row of `other`, which must have the same type.
    pub fn extend(&mut self, other: &VectorData) -> Result<()> {
        let mismatch = || {
            VetaError::internal(format!(
                "cannot append a {} vector to a {} vector",
                other.data_type(),
                self.data_type()
            ))
        };
        if self.data_type() != other.data_type() {
            return Err(mismatch());
        }
        match (self, other) {
            (VectorData::Boolean(a), VectorData::Boolean(b)) => a.extend_from_slice(b),
            (VectorData::Int16(a), VectorData::Int16(b)) => a.extend_from_slice(b),
            (VectorData::Int32(a), VectorData::Int32(b)) => a.extend_from_slice(b),
            (VectorData::Int64(a), VectorData::Int64(b)) => a.extend_from_slice(b),
            (VectorData::Int128(a), VectorData::Int128(b)) => a.extend_from_slice(b),
            (VectorData::Float64(a), VectorData::Float64(b)) => a.extend_from_slice(b),
            (VectorData::Date(a), VectorData::Date(b)) => a.extend_from_slice(b),
            (VectorData::Timestamp(a), VectorData::Timestamp(b)) => a.extend_from_slice(b),
            (VectorData::Decimal64 { values: a, .. }, VectorData::Decimal64 { values: b, .. }) => {
                a.extend_from_slice(b)
            }
            (
                VectorData::Decimal128 { values: a, .. },
                VectorData::Decimal128 { values: b, .. },
            ) => a.extend_from_slice(b),
            (
                VectorData::Varchar { offsets, bytes },
                VectorData::Varchar {
                    offsets: other_offsets,
                    bytes: other_bytes,
                },
            ) => {
                let base = checked_offset(bytes.len())?;
                checked_offset(bytes.len() + other_bytes.len())?;
                bytes.extend_from_slice(other_bytes);
                offsets.extend(other_offsets[1..].iter().map(|o| o + base));
            }
            _ => return Err(VetaError::internal("vector variants differ")),
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Vector {
    pub data: VectorData,
    /// `None`: every row is valid.
    pub validity: Option<Bitmap>,
}

impl Vector {
    pub fn new(data: VectorData, validity: Option<Bitmap>) -> Result<Vector> {
        if let Some(bitmap) = &validity
            && bitmap.len() != data.len()
        {
            return Err(VetaError::internal(format!(
                "validity has {} bits for {} rows",
                bitmap.len(),
                data.len()
            )));
        }
        Ok(Vector { data, validity })
    }

    /// A vector with every row valid.
    pub fn from_data(data: VectorData) -> Vector {
        Vector {
            data,
            validity: None,
        }
    }

    pub fn with_capacity(data_type: DataType, capacity: usize) -> Vector {
        Vector::from_data(VectorData::with_capacity(data_type, capacity))
    }

    /// Builds a vector of `data_type` from values; `Value::Null` becomes a NULL row.
    pub fn from_values(data_type: DataType, values: &[Value]) -> Result<Vector> {
        let mut vector = Vector::with_capacity(data_type, values.len());
        for value in values {
            vector.push(value)?;
        }
        Ok(vector)
    }

    /// `len` copies of `value` (NULL allowed).
    pub fn constant(data_type: DataType, value: &Value, len: usize) -> Result<Vector> {
        let mut vector = Vector::with_capacity(data_type, len);
        for _ in 0..len {
            vector.push(value)?;
        }
        Ok(vector)
    }

    /// `len` NULL rows.
    pub fn nulls(data_type: DataType, len: usize) -> Vector {
        let mut data = VectorData::with_capacity(data_type, len);
        for _ in 0..len {
            data.push_default();
        }
        Vector {
            data,
            validity: Some(Bitmap::new_unset(len)),
        }
    }

    pub fn push(&mut self, value: &Value) -> Result<()> {
        if value.is_null() {
            let len = self.data.len();
            self.data.push_default();
            self.validity
                .get_or_insert_with(|| Bitmap::new_set(len))
                .push(false);
        } else {
            self.data.push_value(value)?;
            if let Some(bitmap) = &mut self.validity {
                bitmap.push(true);
            }
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    pub fn data_type(&self) -> DataType {
        self.data.data_type()
    }

    pub fn is_valid(&self, i: usize) -> bool {
        self.validity.as_ref().is_none_or(|b| b.get(i))
    }

    pub fn is_null(&self, i: usize) -> bool {
        !self.is_valid(i)
    }

    pub fn null_count(&self) -> usize {
        self.validity.as_ref().map_or(0, |b| b.count_unset())
    }

    /// Value at row `i`, `Value::Null` if the row is NULL.
    pub fn get(&self, i: usize) -> Value {
        if self.is_valid(i) {
            self.data.get(i)
        } else {
            Value::Null
        }
    }

    pub fn slice(&self, offset: usize, len: usize) -> Vector {
        Vector {
            data: self.data.slice(offset, len),
            validity: self.validity.as_ref().map(|b| b.slice(offset, len)),
        }
    }

    /// Rows at `indices`, in that order (a selection vector).
    pub fn gather(&self, indices: &[u32]) -> Vector {
        Vector {
            data: self.data.gather(indices),
            validity: self.validity.as_ref().map(|b| b.gather(indices)),
        }
    }

    pub fn extend(&mut self, other: &Vector) -> Result<()> {
        let len = self.len();
        self.data.extend(&other.data)?;
        match (&mut self.validity, &other.validity) {
            (None, None) => {}
            (Some(a), None) => a.extend(&Bitmap::new_set(other.len())),
            (validity @ None, Some(b)) => {
                let mut a = Bitmap::new_set(len);
                a.extend(b);
                *validity = Some(a);
            }
            (Some(a), Some(b)) => a.extend(b),
        }
        Ok(())
    }
}

/// Up to one vector of rows per column; every column has `len` rows.
#[derive(Debug, Clone, PartialEq)]
pub struct Chunk {
    pub columns: Vec<Vector>,
    pub len: usize,
}

impl Chunk {
    /// Chunk whose length is that of its columns (0 without columns).
    pub fn new(columns: Vec<Vector>) -> Result<Chunk> {
        let len = columns.first().map_or(0, Vector::len);
        Chunk::with_len(columns, len)
    }

    /// Chunk with an explicit length, which also allows rows without columns.
    pub fn with_len(columns: Vec<Vector>, len: usize) -> Result<Chunk> {
        if let Some(bad) = columns.iter().find(|c| c.len() != len) {
            return Err(VetaError::internal(format!(
                "chunk column has {} rows, expected {len}",
                bad.len()
            )));
        }
        Ok(Chunk { columns, len })
    }

    /// Empty chunk with one column per type.
    pub fn empty(types: &[DataType]) -> Chunk {
        Chunk {
            columns: types.iter().map(|&t| Vector::with_capacity(t, 0)).collect(),
            len: 0,
        }
    }

    pub fn from_rows(types: &[DataType], rows: &[Vec<Value>]) -> Result<Chunk> {
        let mut columns: Vec<Vector> = types
            .iter()
            .map(|&t| Vector::with_capacity(t, rows.len()))
            .collect();
        for row in rows {
            if row.len() != types.len() {
                return Err(VetaError::internal(format!(
                    "row has {} values, expected {}",
                    row.len(),
                    types.len()
                )));
            }
            for (column, value) in columns.iter_mut().zip(row) {
                column.push(value)?;
            }
        }
        Chunk::with_len(columns, rows.len())
    }

    pub fn num_columns(&self) -> usize {
        self.columns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn column(&self, i: usize) -> &Vector {
        &self.columns[i]
    }

    pub fn row(&self, i: usize) -> Vec<Value> {
        self.columns.iter().map(|c| c.get(i)).collect()
    }

    pub fn slice(&self, offset: usize, len: usize) -> Chunk {
        assert!(offset + len <= self.len, "slice out of range");
        Chunk {
            columns: self.columns.iter().map(|c| c.slice(offset, len)).collect(),
            len,
        }
    }

    pub fn gather(&self, indices: &[u32]) -> Chunk {
        Chunk {
            columns: self.columns.iter().map(|c| c.gather(indices)).collect(),
            len: indices.len(),
        }
    }

    /// Appends the rows of `other`, which must have the same column types.
    pub fn extend(&mut self, other: &Chunk) -> Result<()> {
        if self.columns.len() != other.columns.len() {
            return Err(VetaError::internal("chunks have different column counts"));
        }
        for (a, b) in self.columns.iter_mut().zip(&other.columns) {
            a.extend(b)?;
        }
        self.len += other.len;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(value: i128, precision: u8, scale: u8) -> Value {
        Value::Decimal {
            value,
            precision,
            scale,
        }
    }

    fn all_types() -> Vec<(DataType, Vec<Value>)> {
        vec![
            (
                DataType::Boolean,
                vec![Value::Boolean(true), Value::Null, Value::Boolean(false)],
            ),
            (
                DataType::Int16,
                vec![Value::Int16(-3), Value::Null, Value::Int16(i16::MAX)],
            ),
            (
                DataType::Int32,
                vec![Value::Int32(7), Value::Null, Value::Int32(i32::MIN)],
            ),
            (
                DataType::Int64,
                vec![Value::Int64(1), Value::Null, Value::Int64(-9)],
            ),
            (
                DataType::Int128,
                vec![Value::Int128(i128::MAX), Value::Null, Value::Int128(0)],
            ),
            (
                DataType::Float64,
                vec![Value::Float64(1.5), Value::Null, Value::Float64(-0.25)],
            ),
            (
                DataType::Decimal {
                    precision: 15,
                    scale: 2,
                },
                vec![dec(1234, 15, 2), Value::Null, dec(-5, 15, 2)],
            ),
            (
                DataType::Decimal {
                    precision: 38,
                    scale: 4,
                },
                vec![dec(i128::MAX / 10, 38, 4), Value::Null, dec(1, 38, 4)],
            ),
            (
                DataType::Varchar,
                vec![Value::varchar("abc"), Value::Null, Value::varchar("")],
            ),
            (
                DataType::Date,
                vec![Value::Date(0), Value::Null, Value::Date(-1)],
            ),
            (
                DataType::Timestamp,
                vec![
                    Value::Timestamp(1_372_672_800),
                    Value::Null,
                    Value::Timestamp(-1),
                ],
            ),
        ]
    }

    #[test]
    fn from_values_get_round_trips_every_type() {
        for (data_type, values) in all_types() {
            let vector = Vector::from_values(data_type, &values).unwrap();
            assert_eq!(vector.len(), 3);
            assert_eq!(vector.data_type(), data_type);
            assert_eq!(vector.null_count(), 1);
            for (i, value) in values.iter().enumerate() {
                assert_eq!(&vector.get(i), value, "{data_type} row {i}");
            }
        }
    }

    #[test]
    fn decimal_width_follows_precision() {
        let narrow = Vector::with_capacity(DataType::decimal(18, 2).unwrap(), 0);
        assert!(matches!(narrow.data, VectorData::Decimal64 { .. }));
        let wide = Vector::with_capacity(DataType::decimal(19, 2).unwrap(), 0);
        assert!(matches!(wide.data, VectorData::Decimal128 { .. }));
    }

    #[test]
    fn slice_and_gather_every_type() {
        for (data_type, values) in all_types() {
            let vector = Vector::from_values(data_type, &values).unwrap();
            let sliced = vector.slice(1, 2);
            assert_eq!(sliced.len(), 2);
            assert_eq!(sliced.get(0), Value::Null);
            assert_eq!(sliced.get(1), values[2]);
            let gathered = vector.gather(&[2, 0, 2, 1]);
            let expected = [&values[2], &values[0], &values[2], &values[1]];
            for (i, value) in expected.iter().enumerate() {
                assert_eq!(&gathered.get(i), *value, "{data_type} row {i}");
            }
        }
    }

    #[test]
    fn string_offsets_are_rebased() {
        let values: Vec<Value> = ["a", "bc", "def", "", "ghij"]
            .iter()
            .map(Value::varchar)
            .collect();
        let vector = Vector::from_values(DataType::Varchar, &values).unwrap();
        assert!(vector.validity.is_none());
        let sliced = vector.slice(1, 3);
        match &sliced.data {
            VectorData::Varchar { offsets, bytes } => {
                assert_eq!(offsets, &[0, 2, 5, 5]);
                assert_eq!(bytes, b"bcdef");
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(sliced.data.str_at(1), Some(&b"def"[..]));
    }

    #[test]
    fn push_rejects_wrong_types() {
        let mut vector = Vector::with_capacity(DataType::Int32, 1);
        assert!(vector.push(&Value::Int64(1)).is_err());
        let mut decimal = Vector::with_capacity(DataType::decimal(4, 2).unwrap(), 1);
        assert!(decimal.push(&dec(5, 4, 1)).is_err());
        assert!(decimal.push(&dec(10_000, 5, 2)).is_err());
        assert!(decimal.push(&dec(9_999, 4, 2)).is_ok());
    }

    #[test]
    fn constants_nulls_and_extend() {
        let constant = Vector::constant(DataType::Int64, &Value::Int64(4), 3).unwrap();
        assert_eq!(constant.get(2), Value::Int64(4));
        assert!(constant.validity.is_none());
        let nulls = Vector::nulls(DataType::Varchar, 2);
        assert_eq!(nulls.null_count(), 2);
        assert_eq!(nulls.get(1), Value::Null);

        let mut strings = Vector::from_values(DataType::Varchar, &[Value::varchar("x")]).unwrap();
        strings.extend(&nulls).unwrap();
        strings
            .extend(&Vector::from_values(DataType::Varchar, &[Value::varchar("yz")]).unwrap())
            .unwrap();
        assert_eq!(strings.len(), 4);
        assert_eq!(
            (0..4).map(|i| strings.get(i)).collect::<Vec<_>>(),
            vec![
                Value::varchar("x"),
                Value::Null,
                Value::Null,
                Value::varchar("yz")
            ]
        );
        assert!(strings.extend(&constant).is_err());
        assert!(Vector::new(VectorData::Int64(vec![1, 2]), Some(Bitmap::new_set(3))).is_err());
    }

    #[test]
    fn chunk_rows() {
        let types = [DataType::Int64, DataType::Varchar];
        let rows = vec![
            vec![Value::Int64(1), Value::varchar("a")],
            vec![Value::Null, Value::varchar("b")],
            vec![Value::Int64(3), Value::Null],
        ];
        let chunk = Chunk::from_rows(&types, &rows).unwrap();
        assert_eq!(chunk.len, 3);
        assert_eq!(chunk.num_columns(), 2);
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(&chunk.row(i), row);
        }
        assert_eq!(chunk.slice(1, 2).row(1), rows[2]);
        let gathered = chunk.gather(&[2, 0]);
        assert_eq!(gathered.len, 2);
        assert_eq!(gathered.row(0), rows[2]);
        let mut doubled = chunk.clone();
        doubled.extend(&chunk).unwrap();
        assert_eq!(doubled.len, 6);
        assert_eq!(doubled.row(4), rows[1]);
        assert!(Chunk::from_rows(&types, &[vec![Value::Int64(1)]]).is_err());
        assert!(
            Chunk::new(vec![
                Vector::nulls(DataType::Int64, 1),
                Vector::nulls(DataType::Int64, 2)
            ])
            .is_err()
        );
        assert_eq!(Chunk::with_len(Vec::new(), 5).unwrap().len, 5);
        assert!(Chunk::empty(&types).is_empty());
    }
}
