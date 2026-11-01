//! Column and table statistics for pruning and cardinality estimation (contract §5.2, §11.5).

use crate::error::{Result, VetaError};
use crate::hll::{HLL_REGISTERS, Hll};
use crate::types::{DataType, MAX_DECIMAL64_PRECISION};
use crate::value::Value;
use crate::vector::{Chunk, Vector};

/// Maximum rows of a part sample (contract §5.2).
pub const SAMPLE_ROWS: u64 = 1024;
/// Strings longer than this do not get a persisted min/max (contract §5.2).
pub const MAX_MINMAX_STRING: usize = 256;

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnStats {
    pub rows: u64,
    pub nulls: u64,
    /// Minimum non-NULL value; `None` if there is none or it is not kept.
    pub min: Option<Value>,
    pub max: Option<Value>,
    pub hll: Hll,
}

impl Default for ColumnStats {
    fn default() -> Self {
        ColumnStats::new()
    }
}

impl ColumnStats {
    pub fn new() -> Self {
        ColumnStats {
            rows: 0,
            nulls: 0,
            min: None,
            max: None,
            hll: Hll::new(),
        }
    }

    pub fn update(&mut self, value: &Value) {
        self.rows += 1;
        if value.is_null() {
            self.nulls += 1;
            return;
        }
        self.hll.add_value(value);
        if self.min.as_ref().is_none_or(|m| value < m) {
            self.min = Some(value.clone());
        }
        if self.max.as_ref().is_none_or(|m| value > m) {
            self.max = Some(value.clone());
        }
    }

    pub fn update_vector(&mut self, vector: &Vector) {
        for i in 0..vector.len() {
            self.update(&vector.get(i));
        }
    }

    pub fn merge(&mut self, other: &ColumnStats) {
        self.rows += other.rows;
        self.nulls += other.nulls;
        self.hll.merge(&other.hll);
        if let Some(m) = &other.min
            && self.min.as_ref().is_none_or(|s| m < s)
        {
            self.min = Some(m.clone());
        }
        if let Some(m) = &other.max
            && self.max.as_ref().is_none_or(|s| m > s)
        {
            self.max = Some(m.clone());
        }
    }

    /// Distinct non-NULL values: the HLL estimate capped at the non-NULL row count (§11.5).
    pub fn ndv(&self) -> f64 {
        self.hll.estimate().min((self.rows - self.nulls) as f64)
    }

    /// `nulls / rows`, 0 for an empty column (§11.5).
    pub fn null_fraction(&self) -> f64 {
        if self.rows == 0 {
            0.0
        } else {
            self.nulls as f64 / self.rows as f64
        }
    }

    /// Whether min/max are persisted: some non-NULL value exists and neither bound is a
    /// string longer than 256 bytes (§5.2).
    pub fn has_minmax(&self) -> bool {
        let short = |v: &Option<Value>| match v {
            Some(Value::Varchar(b)) => b.len() <= MAX_MINMAX_STRING,
            Some(_) => true,
            None => false,
        };
        short(&self.min) && short(&self.max)
    }

    /// `stats.bin` record of one column: `rows u64`, `nulls u64`, `has_minmax u8`, `min`,
    /// `max`, 4096 HLL registers (§5.2).
    pub fn encode(&self, data_type: DataType, out: &mut Vec<u8>) -> Result<()> {
        out.extend_from_slice(&self.rows.to_le_bytes());
        out.extend_from_slice(&self.nulls.to_le_bytes());
        match (&self.min, &self.max) {
            (Some(min), Some(max)) if self.has_minmax() => {
                out.push(1);
                encode_value(min, data_type, out)?;
                encode_value(max, data_type, out)?;
            }
            _ => out.push(0),
        }
        out.extend_from_slice(self.hll.registers());
        Ok(())
    }

    /// Reads a record written by [`ColumnStats::encode`] at `*pos`, advancing it.
    pub fn decode(bytes: &[u8], pos: &mut usize, data_type: DataType) -> Result<ColumnStats> {
        let rows = u64::from_le_bytes(take::<8>(bytes, pos)?);
        let nulls = u64::from_le_bytes(take::<8>(bytes, pos)?);
        let has_minmax = take::<1>(bytes, pos)?[0];
        let (min, max) = match has_minmax {
            0 => (None, None),
            1 => (
                Some(decode_value(bytes, pos, data_type)?),
                Some(decode_value(bytes, pos, data_type)?),
            ),
            other => return Err(truncated(&format!("has_minmax = {other}"))),
        };
        let registers = take_slice(bytes, pos, HLL_REGISTERS)?;
        Ok(ColumnStats {
            rows,
            nulls,
            min,
            max,
            hll: Hll::from_bytes(registers)?,
        })
    }
}

fn truncated(what: &str) -> VetaError {
    VetaError::internal(format!("invalid stats encoding: {what}"))
}

fn take_slice<'a>(bytes: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8]> {
    let end = pos
        .checked_add(n)
        .filter(|&e| e <= bytes.len())
        .ok_or_else(|| truncated("truncated"))?;
    let slice = &bytes[*pos..end];
    *pos = end;
    Ok(slice)
}

fn take<const N: usize>(bytes: &[u8], pos: &mut usize) -> Result<[u8; N]> {
    let slice = take_slice(bytes, pos, N)?;
    Ok(slice.try_into().expect("length checked"))
}

/// Value encoding of `stats.bin`: the physical representation of §4 in little-endian
/// (`i128` in 16 bytes, booleans in 1 byte), strings as `u32` LE length + bytes.
pub fn encode_value(value: &Value, data_type: DataType, out: &mut Vec<u8>) -> Result<()> {
    match (data_type, value) {
        (DataType::Boolean, Value::Boolean(b)) => out.push(*b as u8),
        (DataType::Int16, Value::Int16(v)) => out.extend_from_slice(&v.to_le_bytes()),
        (DataType::Int32, Value::Int32(v)) => out.extend_from_slice(&v.to_le_bytes()),
        (DataType::Int64, Value::Int64(v)) => out.extend_from_slice(&v.to_le_bytes()),
        (DataType::Int128, Value::Int128(v)) => out.extend_from_slice(&v.to_le_bytes()),
        (DataType::Float64, Value::Float64(v)) => out.extend_from_slice(&v.to_le_bytes()),
        (DataType::Date, Value::Date(v)) => out.extend_from_slice(&v.to_le_bytes()),
        (DataType::Timestamp, Value::Timestamp(v)) => out.extend_from_slice(&v.to_le_bytes()),
        (
            DataType::Decimal { precision, scale },
            Value::Decimal {
                value, scale: s, ..
            },
        ) if *s == scale => {
            if precision <= MAX_DECIMAL64_PRECISION {
                let narrow = i64::try_from(*value)
                    .map_err(|_| VetaError::internal("decimal does not fit i64"))?;
                out.extend_from_slice(&narrow.to_le_bytes());
            } else {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
        (DataType::Varchar, Value::Varchar(bytes)) => {
            let len = u32::try_from(bytes.len())
                .map_err(|_| VetaError::internal("string exceeds 4 GiB"))?;
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(bytes);
        }
        _ => {
            return Err(VetaError::internal(format!(
                "cannot encode {value:?} as {data_type}"
            )));
        }
    }
    Ok(())
}

/// Reads a value written by [`encode_value`] at `*pos`, advancing it.
pub fn decode_value(bytes: &[u8], pos: &mut usize, data_type: DataType) -> Result<Value> {
    Ok(match data_type {
        DataType::Boolean => Value::Boolean(take::<1>(bytes, pos)?[0] != 0),
        DataType::Int16 => Value::Int16(i16::from_le_bytes(take(bytes, pos)?)),
        DataType::Int32 => Value::Int32(i32::from_le_bytes(take(bytes, pos)?)),
        DataType::Int64 => Value::Int64(i64::from_le_bytes(take(bytes, pos)?)),
        DataType::Int128 => Value::Int128(i128::from_le_bytes(take(bytes, pos)?)),
        DataType::Float64 => Value::Float64(f64::from_le_bytes(take(bytes, pos)?)),
        DataType::Date => Value::Date(i32::from_le_bytes(take(bytes, pos)?)),
        DataType::Timestamp => Value::Timestamp(i64::from_le_bytes(take(bytes, pos)?)),
        DataType::Decimal { precision, scale } => {
            let value = if precision <= MAX_DECIMAL64_PRECISION {
                i64::from_le_bytes(take(bytes, pos)?) as i128
            } else {
                i128::from_le_bytes(take(bytes, pos)?)
            };
            Value::Decimal {
                value,
                precision,
                scale,
            }
        }
        DataType::Varchar => {
            let len = u32::from_le_bytes(take(bytes, pos)?) as usize;
            Value::Varchar(take_slice(bytes, pos, len)?.to_vec())
        }
    })
}

/// Row positions of the systematic sample of a part: `s = min(1024, rows)` rows at
/// `floor(i · rows / s)` (contract §5.2).
pub fn sample_positions(rows: u64) -> Vec<u64> {
    let s = rows.min(SAMPLE_ROWS);
    (0..s)
        .map(|i| (i as u128 * rows as u128 / s as u128) as u64)
        .collect()
}

/// Statistics of a table (or of one part): row count, per-column stats, one HLL per
/// primary-key prefix (lengths `1..=k`) and the row sample.
#[derive(Debug, Clone, PartialEq)]
pub struct TableStats {
    pub rows: u64,
    pub columns: Vec<ColumnStats>,
    pub key_prefix_hll: Vec<Hll>,
    pub sample: Chunk,
}

impl TableStats {
    pub fn new(types: &[DataType], key_columns: usize) -> TableStats {
        TableStats {
            rows: 0,
            columns: vec![ColumnStats::new(); types.len()],
            key_prefix_hll: vec![Hll::new(); key_columns],
            sample: Chunk::empty(types),
        }
    }

    /// Adds the rows of `chunk` to the row count and the column stats.
    pub fn update_chunk(&mut self, chunk: &Chunk) -> Result<()> {
        if chunk.num_columns() != self.columns.len() {
            return Err(VetaError::internal("chunk does not match the table stats"));
        }
        self.rows += chunk.len as u64;
        for (stats, column) in self.columns.iter_mut().zip(&chunk.columns) {
            stats.update_vector(column);
        }
        Ok(())
    }

    /// Union of two statistics: sums, HLL unions and the concatenation of the samples
    /// (the union of the part samples of §11.5).
    pub fn merge(&mut self, other: &TableStats) -> Result<()> {
        if other.columns.len() != self.columns.len()
            || other.key_prefix_hll.len() != self.key_prefix_hll.len()
        {
            return Err(VetaError::internal("table stats have different shapes"));
        }
        self.rows += other.rows;
        for (a, b) in self.columns.iter_mut().zip(&other.columns) {
            a.merge(b);
        }
        for (a, b) in self.key_prefix_hll.iter_mut().zip(&other.key_prefix_hll) {
            a.merge(b);
        }
        self.sample.extend(&other.sample)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int_vector(values: &[Option<i64>]) -> Vector {
        let values: Vec<Value> = values
            .iter()
            .map(|v| v.map_or(Value::Null, Value::Int64))
            .collect();
        Vector::from_values(DataType::Int64, &values).unwrap()
    }

    #[test]
    fn column_stats_track_rows_nulls_and_bounds() {
        let mut stats = ColumnStats::new();
        stats.update_vector(&int_vector(&[
            Some(5),
            None,
            Some(-2),
            Some(9),
            None,
            Some(5),
        ]));
        assert_eq!(stats.rows, 6);
        assert_eq!(stats.nulls, 2);
        assert_eq!(stats.min, Some(Value::Int64(-2)));
        assert_eq!(stats.max, Some(Value::Int64(9)));
        assert!((stats.ndv() - 3.0).abs() < 0.5);
        assert!((stats.null_fraction() - 1.0 / 3.0).abs() < 1e-12);

        let mut all_null = ColumnStats::new();
        all_null.update_vector(&int_vector(&[None, None]));
        assert_eq!(all_null.min, None);
        assert!(!all_null.has_minmax());
        assert_eq!(all_null.ndv(), 0.0);
    }

    #[test]
    fn merge_combines_parts() {
        let mut a = ColumnStats::new();
        a.update_vector(&int_vector(&[Some(1), Some(2), None]));
        let mut b = ColumnStats::new();
        b.update_vector(&int_vector(&[Some(-4), Some(2)]));
        let mut whole = ColumnStats::new();
        whole.update_vector(&int_vector(&[Some(1), Some(2), None, Some(-4), Some(2)]));
        a.merge(&b);
        assert_eq!(a, whole);
    }

    #[test]
    fn encode_decode_round_trip() {
        let cases = [
            (DataType::Int64, vec![Value::Int64(3), Value::Int64(-8)]),
            (DataType::Int16, vec![Value::Int16(3), Value::Null]),
            (
                DataType::Boolean,
                vec![Value::Boolean(false), Value::Boolean(true)],
            ),
            (
                DataType::decimal(15, 2).unwrap(),
                vec![Value::Decimal {
                    value: -12345,
                    precision: 15,
                    scale: 2,
                }],
            ),
            (
                DataType::decimal(38, 2).unwrap(),
                vec![Value::Decimal {
                    value: i128::MAX / 100,
                    precision: 38,
                    scale: 2,
                }],
            ),
            (
                DataType::Varchar,
                vec![Value::varchar("b"), Value::varchar("abc")],
            ),
            (DataType::Date, vec![Value::Date(-1)]),
            (DataType::Timestamp, vec![Value::Timestamp(1_372_672_800)]),
            (DataType::Float64, vec![Value::Float64(2.5)]),
            (DataType::Int32, vec![Value::Null]),
        ];
        for (data_type, values) in cases {
            let mut stats = ColumnStats::new();
            stats.update_vector(&Vector::from_values(data_type, &values).unwrap());
            let mut bytes = Vec::new();
            stats.encode(data_type, &mut bytes).unwrap();
            let mut pos = 0;
            let decoded = ColumnStats::decode(&bytes, &mut pos, data_type).unwrap();
            assert_eq!(pos, bytes.len());
            assert_eq!(decoded, stats, "{data_type}");
            let mut short = 0;
            assert!(ColumnStats::decode(&bytes[..bytes.len() - 1], &mut short, data_type).is_err());
        }
    }

    #[test]
    fn long_strings_drop_minmax() {
        let mut stats = ColumnStats::new();
        stats.update(&Value::Varchar(vec![b'x'; 257]));
        stats.update(&Value::varchar("a"));
        assert!(!stats.has_minmax());
        let mut bytes = Vec::new();
        stats.encode(DataType::Varchar, &mut bytes).unwrap();
        assert_eq!(bytes.len(), 8 + 8 + 1 + 4096);
        let decoded = ColumnStats::decode(&bytes, &mut 0, DataType::Varchar).unwrap();
        assert_eq!(decoded.min, None);
        assert_eq!(decoded.rows, 2);
    }

    #[test]
    fn systematic_sample() {
        assert_eq!(sample_positions(0), Vec::<u64>::new());
        assert_eq!(sample_positions(3), vec![0, 1, 2]);
        let positions = sample_positions(10_000);
        assert_eq!(positions.len(), 1024);
        assert_eq!(positions[0], 0);
        assert_eq!(positions[1], 9);
        assert_eq!(positions[1023], 1023 * 10_000 / 1024);
        assert!(positions.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn table_stats_merge() {
        let types = [DataType::Int64, DataType::Varchar];
        let chunk_a =
            Chunk::from_rows(&types, &[vec![Value::Int64(1), Value::varchar("a")]]).unwrap();
        let chunk_b = Chunk::from_rows(
            &types,
            &[
                vec![Value::Int64(2), Value::Null],
                vec![Value::Int64(3), Value::varchar("c")],
            ],
        )
        .unwrap();
        let mut a = TableStats::new(&types, 1);
        a.update_chunk(&chunk_a).unwrap();
        a.sample = chunk_a.clone();
        let mut b = TableStats::new(&types, 1);
        b.update_chunk(&chunk_b).unwrap();
        b.sample = chunk_b.clone();
        a.merge(&b).unwrap();
        assert_eq!(a.rows, 3);
        assert_eq!(a.columns[0].max, Some(Value::Int64(3)));
        assert_eq!(a.columns[1].nulls, 1);
        assert_eq!(a.sample.len, 3);
        assert_eq!(a.sample.row(1), vec![Value::Int64(2), Value::Null]);
        assert!(a.merge(&TableStats::new(&types[..1], 1)).is_err());
    }
}
