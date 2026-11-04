//! Physical form of the group keys (contract §12): fixed-width keys packed in `u64`/`u128`
//! when they fit and strings or wide keys as bytes otherwise.
//!
//! In a packed key each column takes its physical width and a nullable column one more
//! byte whose value 1 means NULL; the value bits of a NULL are zero. A composite key is, for
//! each column in order, a flag byte if the column is nullable (0 = NULL, then no value) and
//! the value: fixed-width in little-endian, strings as `u32` LE length + bytes. Floats are
//! canonical in both forms (`-0.0` is `0.0`, one NaN), so equal values share a group.

use std::borrow::Cow;

use veta_core::types::MAX_DECIMAL64_PRECISION;
use veta_core::{Bitmap, DataType, Result, Vector, VectorData, VetaError};

use crate::hash_table::{BytesTable, FixedKey};

/// One column of a packed key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FixedColumn {
    pub data_type: DataType,
    /// Bit where the value starts.
    pub shift: u32,
    /// Bytes of the value.
    pub width: u32,
    /// Bit of the NULL flag of a nullable column.
    pub null_shift: Option<u32>,
}

/// How the keys of an aggregation are represented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum KeyLayout {
    /// No `GROUP BY`: a single group.
    Global,
    Fixed64(Vec<FixedColumn>),
    Fixed128(Vec<FixedColumn>),
    /// One NOT NULL string column, used as is.
    Str,
    /// `(type, nullable)` of each column of a composite key.
    Composite(Vec<(DataType, bool)>),
}

impl KeyLayout {
    /// Picks the narrowest layout for group columns of these types.
    pub fn new(columns: &[(DataType, bool)]) -> KeyLayout {
        if columns.is_empty() {
            return KeyLayout::Global;
        }
        if let [(DataType::Varchar, false)] = columns {
            return KeyLayout::Str;
        }
        let mut fixed = Vec::with_capacity(columns.len());
        let mut bits = 0u32;
        for &(data_type, nullable) in columns {
            let Some(width) = data_type.fixed_width() else {
                return KeyLayout::Composite(columns.to_vec());
            };
            let width = width as u32;
            let shift = bits;
            bits += width * 8;
            let null_shift = nullable.then_some(bits);
            if nullable {
                bits += 8;
            }
            if bits > 128 {
                return KeyLayout::Composite(columns.to_vec());
            }
            fixed.push(FixedColumn {
                data_type,
                shift,
                width,
                null_shift,
            });
        }
        if bits <= 64 {
            KeyLayout::Fixed64(fixed)
        } else {
            KeyLayout::Fixed128(fixed)
        }
    }
}

/// `f64` with one bit pattern per grouping value.
#[inline]
pub(crate) fn canonical_bits(f: f64) -> u64 {
    if f.is_nan() {
        f64::NAN.to_bits()
    } else if f == 0.0 {
        0
    } else {
        f.to_bits()
    }
}

fn or_column<K: FixedKey, T>(
    keys: &mut [K],
    values: &[T],
    validity: Option<&Bitmap>,
    column: &FixedColumn,
    f: impl Fn(&T) -> u128,
) -> Result<()> {
    match validity {
        None => {
            for (key, value) in keys.iter_mut().zip(values) {
                *key = key.or_shifted(f(value), column.shift);
            }
        }
        Some(bitmap) => {
            for (i, (key, value)) in keys.iter_mut().zip(values).enumerate() {
                if bitmap.get(i) {
                    *key = key.or_shifted(f(value), column.shift);
                } else if let Some(null_shift) = column.null_shift {
                    *key = key.or_shifted(1, null_shift);
                } else {
                    return Err(VetaError::internal("NULL in a NOT NULL group column"));
                }
            }
        }
    }
    Ok(())
}

fn pack_column<K: FixedKey>(keys: &mut [K], column: &FixedColumn, v: &Vector) -> Result<()> {
    let validity = v.validity.as_ref();
    match &v.data {
        VectorData::Boolean(x) => or_column(keys, x, validity, column, |a: &bool| *a as u128),
        VectorData::Int16(x) => or_column(keys, x, validity, column, |a: &i16| *a as u16 as u128),
        VectorData::Int32(x) | VectorData::Date(x) => {
            or_column(keys, x, validity, column, |a: &i32| *a as u32 as u128)
        }
        VectorData::Int64(x) | VectorData::Timestamp(x) => {
            or_column(keys, x, validity, column, |a: &i64| *a as u64 as u128)
        }
        VectorData::Decimal64 { values, .. } => {
            or_column(keys, values, validity, column, |a: &i64| *a as u64 as u128)
        }
        VectorData::Int128(x) => or_column(keys, x, validity, column, |a: &i128| *a as u128),
        VectorData::Decimal128 { values, .. } => {
            or_column(keys, values, validity, column, |a: &i128| *a as u128)
        }
        VectorData::Float64(x) => or_column(keys, x, validity, column, |a: &f64| {
            canonical_bits(*a) as u128
        }),
        VectorData::Varchar { .. } => Err(VetaError::internal("string in a packed group key")),
    }
}

/// Packs `n` rows of the group columns into `keys`.
pub(crate) fn pack_keys<K: FixedKey>(
    columns: &[FixedColumn],
    vectors: &[Cow<'_, Vector>],
    n: usize,
    keys: &mut Vec<K>,
) -> Result<()> {
    keys.clear();
    keys.resize(n, K::default());
    for (column, vector) in columns.iter().zip(vectors) {
        pack_column(keys, column, vector)?;
    }
    Ok(())
}

/// The column `column` of the packed `keys`.
pub(crate) fn unpack_column<K: FixedKey>(keys: &[K], column: &FixedColumn) -> Result<Vector> {
    let mask = if column.width >= 16 {
        u128::MAX
    } else {
        (1u128 << (column.width * 8)) - 1
    };
    let raw = |key: &K| (key.bits() >> column.shift) & mask;
    let data = match column.data_type {
        DataType::Boolean => VectorData::Boolean(keys.iter().map(|k| raw(k) != 0).collect()),
        DataType::Int16 => VectorData::Int16(keys.iter().map(|k| raw(k) as u16 as i16).collect()),
        DataType::Int32 => VectorData::Int32(keys.iter().map(|k| raw(k) as u32 as i32).collect()),
        DataType::Date => VectorData::Date(keys.iter().map(|k| raw(k) as u32 as i32).collect()),
        DataType::Int64 => VectorData::Int64(keys.iter().map(|k| raw(k) as u64 as i64).collect()),
        DataType::Timestamp => {
            VectorData::Timestamp(keys.iter().map(|k| raw(k) as u64 as i64).collect())
        }
        DataType::Int128 => VectorData::Int128(keys.iter().map(|k| raw(k) as i128).collect()),
        DataType::Float64 => {
            VectorData::Float64(keys.iter().map(|k| f64::from_bits(raw(k) as u64)).collect())
        }
        DataType::Decimal { precision, scale } if precision <= MAX_DECIMAL64_PRECISION => {
            VectorData::Decimal64 {
                values: keys.iter().map(|k| raw(k) as u64 as i64).collect(),
                precision,
                scale,
            }
        }
        DataType::Decimal { precision, scale } => VectorData::Decimal128 {
            values: keys.iter().map(|k| raw(k) as i128).collect(),
            precision,
            scale,
        },
        DataType::Varchar => return Err(VetaError::internal("string in a packed group key")),
    };
    let validity = column.null_shift.map(|shift| {
        let valid: Vec<bool> = keys.iter().map(|k| (k.bits() >> shift) & 1 == 0).collect();
        Bitmap::from_bools(&valid)
    });
    Vector::new(data, validity)
}

/// Row keys of one batch as bytes; `cursors` is scratch space for the writers, which is also
/// what the sort keys of [`crate::sort`] are written with.
#[derive(Debug, Default)]
pub(crate) struct KeyBuf {
    pub offsets: Vec<usize>,
    pub bytes: Vec<u8>,
    pub cursors: Vec<usize>,
}

impl KeyBuf {
    pub fn key(&self, i: usize) -> &[u8] {
        &self.bytes[self.offsets[i]..self.offsets[i + 1]]
    }

    /// Starts `n` rows of unknown size: every size is zero until [`KeyBuf::add_size`].
    pub fn begin(&mut self, n: usize) {
        self.offsets.clear();
        self.offsets.resize(n + 1, 0);
    }

    #[inline]
    pub fn add_size(&mut self, row: usize, size: usize) {
        self.offsets[row + 1] += size;
    }

    /// Turns the sizes into offsets and prepares the buffer for the writers.
    pub fn allocate(&mut self) {
        let n = self.offsets.len() - 1;
        for i in 0..n {
            self.offsets[i + 1] += self.offsets[i];
        }
        self.bytes.clear();
        self.bytes.resize(self.offsets[n], 0);
        self.cursors.clear();
        self.cursors.extend_from_slice(&self.offsets[..n]);
    }
}

fn write_fixed<T: Copy, const N: usize>(
    values: &[T],
    v: &Vector,
    nullable: bool,
    buf: &mut KeyBuf,
    f: impl Fn(T) -> [u8; N],
) {
    for (i, &value) in values.iter().enumerate() {
        let cursor = &mut buf.cursors[i];
        let valid = v.is_valid(i);
        if nullable {
            buf.bytes[*cursor] = valid as u8;
            *cursor += 1;
        }
        if valid {
            buf.bytes[*cursor..*cursor + N].copy_from_slice(&f(value));
            *cursor += N;
        }
    }
}

/// Serializes `n` rows of the group columns as composite keys.
pub(crate) fn serialize_keys(
    columns: &[(DataType, bool)],
    vectors: &[Cow<'_, Vector>],
    n: usize,
    buf: &mut KeyBuf,
) -> Result<()> {
    buf.begin(n);
    for (&(data_type, nullable), v) in columns.iter().zip(vectors) {
        if !nullable && v.null_count() > 0 {
            return Err(VetaError::internal("NULL in a NOT NULL group column"));
        }
        let flag = nullable as usize;
        match &v.data {
            VectorData::Varchar { offsets, .. } => {
                for i in 0..n {
                    let len = if v.is_valid(i) {
                        4 + (offsets[i + 1] - offsets[i]) as usize
                    } else {
                        0
                    };
                    buf.add_size(i, flag + len);
                }
            }
            _ => {
                let width = data_type.fixed_width().unwrap_or(0);
                for i in 0..n {
                    let len = if v.is_valid(i) { width } else { 0 };
                    buf.add_size(i, flag + len);
                }
            }
        }
    }
    buf.allocate();
    for (&(_, nullable), vector) in columns.iter().zip(vectors) {
        let v: &Vector = vector;
        match &v.data {
            VectorData::Boolean(x) => write_fixed(x, v, nullable, buf, |a: bool| [a as u8]),
            VectorData::Int16(x) => write_fixed(x, v, nullable, buf, |a: i16| a.to_le_bytes()),
            VectorData::Int32(x) | VectorData::Date(x) => {
                write_fixed(x, v, nullable, buf, |a: i32| a.to_le_bytes())
            }
            VectorData::Int64(x) | VectorData::Timestamp(x) => {
                write_fixed(x, v, nullable, buf, |a: i64| a.to_le_bytes())
            }
            VectorData::Decimal64 { values, .. } => {
                write_fixed(values, v, nullable, buf, |a: i64| a.to_le_bytes())
            }
            VectorData::Int128(x) => write_fixed(x, v, nullable, buf, |a: i128| a.to_le_bytes()),
            VectorData::Decimal128 { values, .. } => {
                write_fixed(values, v, nullable, buf, |a: i128| a.to_le_bytes())
            }
            VectorData::Float64(x) => write_fixed(x, v, nullable, buf, |a: f64| {
                canonical_bits(a).to_le_bytes()
            }),
            VectorData::Varchar { offsets, bytes } => {
                for i in 0..n {
                    let cursor = &mut buf.cursors[i];
                    let valid = v.is_valid(i);
                    if nullable {
                        buf.bytes[*cursor] = valid as u8;
                        *cursor += 1;
                    }
                    if valid {
                        let value = &bytes[offsets[i] as usize..offsets[i + 1] as usize];
                        let len = value.len() as u32;
                        buf.bytes[*cursor..*cursor + 4].copy_from_slice(&len.to_le_bytes());
                        *cursor += 4;
                        buf.bytes[*cursor..*cursor + value.len()].copy_from_slice(value);
                        *cursor += value.len();
                    }
                }
            }
        }
    }
    Ok(())
}

fn take<const N: usize>(key: &[u8], pos: &mut usize) -> [u8; N] {
    let bytes: [u8; N] = key[*pos..*pos + N]
        .try_into()
        .expect("composite key layout");
    *pos += N;
    bytes
}

fn read_value(out: &mut VectorData, key: &[u8], pos: &mut usize) -> Result<()> {
    match out {
        VectorData::Boolean(v) => v.push(take::<1>(key, pos)[0] != 0),
        VectorData::Int16(v) => v.push(i16::from_le_bytes(take(key, pos))),
        VectorData::Int32(v) | VectorData::Date(v) => v.push(i32::from_le_bytes(take(key, pos))),
        VectorData::Int64(v) | VectorData::Timestamp(v) => {
            v.push(i64::from_le_bytes(take(key, pos)))
        }
        VectorData::Int128(v) => v.push(i128::from_le_bytes(take(key, pos))),
        VectorData::Float64(v) => v.push(f64::from_bits(u64::from_le_bytes(take(key, pos)))),
        VectorData::Decimal64 { values, .. } => values.push(i64::from_le_bytes(take(key, pos))),
        VectorData::Decimal128 { values, .. } => values.push(i128::from_le_bytes(take(key, pos))),
        VectorData::Varchar { offsets, bytes } => {
            let len = u32::from_le_bytes(take(key, pos)) as usize;
            bytes.extend_from_slice(&key[*pos..*pos + len]);
            *pos += len;
            let end = u32::try_from(bytes.len())
                .map_err(|_| VetaError::internal("string vector exceeds 4 GiB"))?;
            offsets.push(end);
        }
    }
    Ok(())
}

/// The group columns of every composite key of `table`, in id order.
pub(crate) fn decode_composite(
    columns: &[(DataType, bool)],
    table: &BytesTable,
) -> Result<Vec<Vector>> {
    let n = table.len();
    let mut data: Vec<VectorData> = columns
        .iter()
        .map(|&(t, _)| VectorData::with_capacity(t, n))
        .collect();
    let mut valid: Vec<Vec<bool>> = columns.iter().map(|_| Vec::with_capacity(n)).collect();
    for id in 0..n {
        let key = table.key(id);
        let mut pos = 0;
        for (c, &(_, nullable)) in columns.iter().enumerate() {
            if nullable {
                let flag = key[pos];
                pos += 1;
                if flag == 0 {
                    data[c].push_default();
                    valid[c].push(false);
                    continue;
                }
            }
            read_value(&mut data[c], key, &mut pos)?;
            valid[c].push(true);
        }
    }
    data.into_iter()
        .zip(valid)
        .zip(columns)
        .map(|((data, valid), &(_, nullable))| {
            let validity = nullable.then(|| Bitmap::from_bools(&valid));
            Vector::new(data, validity)
        })
        .collect()
}

/// Every key of `table` as one string column, in id order.
pub(crate) fn decode_strings(table: &BytesTable) -> Result<Vector> {
    let n = table.len();
    let mut offsets = Vec::with_capacity(n + 1);
    let mut bytes = Vec::new();
    offsets.push(0u32);
    for id in 0..n {
        bytes.extend_from_slice(table.key(id));
        offsets.push(
            u32::try_from(bytes.len())
                .map_err(|_| VetaError::internal("string vector exceeds 4 GiB"))?,
        );
    }
    Ok(Vector::from_data(VectorData::Varchar { offsets, bytes }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash_table::FixedTable;
    use veta_core::Value;
    use veta_core::hash::hash_bytes;

    fn cows(vectors: &[Vector]) -> Vec<Cow<'_, Vector>> {
        vectors.iter().map(Cow::Borrowed).collect()
    }

    #[test]
    fn layouts_follow_the_key_width() {
        use DataType::*;
        assert_eq!(KeyLayout::new(&[]), KeyLayout::Global);
        assert_eq!(KeyLayout::new(&[(Varchar, false)]), KeyLayout::Str);
        assert!(matches!(
            KeyLayout::new(&[(Varchar, true)]),
            KeyLayout::Composite(_)
        ));
        assert!(matches!(
            KeyLayout::new(&[(Int64, false)]),
            KeyLayout::Fixed64(_)
        ));
        assert!(matches!(
            KeyLayout::new(&[(Int32, true), (Int16, true)]),
            KeyLayout::Fixed64(_)
        ));
        assert!(matches!(
            KeyLayout::new(&[(Int64, true)]),
            KeyLayout::Fixed128(_)
        ));
        assert!(matches!(
            KeyLayout::new(&[(Int64, false), (Int32, false)]),
            KeyLayout::Fixed128(_)
        ));
        assert!(matches!(
            KeyLayout::new(&[(Int128, true)]),
            KeyLayout::Composite(_)
        ));
        assert!(matches!(
            KeyLayout::new(&[(Int64, false), (Varchar, false)]),
            KeyLayout::Composite(_)
        ));
    }

    fn sample_columns() -> Vec<Vector> {
        vec![
            Vector::from_values(
                DataType::Int32,
                &[Value::Int32(-1), Value::Null, Value::Int32(7), Value::Null],
            )
            .unwrap(),
            Vector::from_values(
                DataType::Boolean,
                &[
                    Value::Boolean(true),
                    Value::Boolean(false),
                    Value::Boolean(true),
                    Value::Boolean(false),
                ],
            )
            .unwrap(),
            Vector::from_values(
                DataType::Float64,
                &[
                    Value::Float64(-0.0),
                    Value::Float64(1.5),
                    Value::Null,
                    Value::Float64(1.5),
                ],
            )
            .unwrap(),
        ]
    }

    #[test]
    fn packed_keys_round_trip_and_group_equal_rows() {
        let vectors = sample_columns();
        let layout = KeyLayout::new(&[
            (DataType::Int32, true),
            (DataType::Boolean, false),
            (DataType::Float64, true),
        ]);
        let KeyLayout::Fixed128(columns) = layout else {
            panic!("expected a 128-bit layout");
        };
        let mut keys: Vec<u128> = Vec::new();
        pack_keys(&columns, &cows(&vectors), 4, &mut keys).unwrap();
        assert_eq!(keys[1], keys[3], "rows 1 and 3 are the same group");
        assert_ne!(keys[0], keys[2]);

        let mut table = FixedTable::<u128>::with_slots(8);
        let ids: Vec<u32> = keys.iter().map(|&k| table.find_or_insert(k)).collect();
        assert_eq!(ids, vec![0, 1, 2, 1]);

        let a = unpack_column(&keys, &columns[0]).unwrap();
        assert_eq!(a.get(0), Value::Int32(-1));
        assert_eq!(a.get(1), Value::Null);
        assert_eq!(a.get(2), Value::Int32(7));
        let b = unpack_column(&keys, &columns[1]).unwrap();
        assert_eq!(b.get(2), Value::Boolean(true));
        let c = unpack_column(&keys, &columns[2]).unwrap();
        assert_eq!(c.get(0), Value::Float64(0.0));
        assert_eq!(c.get(2), Value::Null);
    }

    #[test]
    fn a_null_in_a_not_null_key_is_an_error() {
        let vectors = vec![Vector::nulls(DataType::Int32, 2)];
        let KeyLayout::Fixed64(columns) = KeyLayout::new(&[(DataType::Int32, false)]) else {
            panic!("expected a 64-bit layout");
        };
        let mut keys: Vec<u64> = Vec::new();
        assert!(pack_keys(&columns, &cows(&vectors), 2, &mut keys).is_err());
    }

    #[test]
    fn composite_keys_round_trip() {
        let columns = [(DataType::Varchar, true), (DataType::Int64, false)];
        let vectors = vec![
            Vector::from_values(
                DataType::Varchar,
                &[
                    Value::varchar("ab"),
                    Value::Null,
                    Value::varchar(""),
                    Value::varchar("ab"),
                ],
            )
            .unwrap(),
            Vector::from_data(VectorData::Int64(vec![1, 2, 3, 1])),
        ];
        let mut buf = KeyBuf::default();
        serialize_keys(&columns, &cows(&vectors), 4, &mut buf).unwrap();
        assert_eq!(buf.key(0), buf.key(3));
        assert_ne!(buf.key(0), buf.key(1));

        let mut table = BytesTable::with_slots(8);
        for i in 0..4 {
            table.find_or_insert(buf.key(i), hash_bytes(buf.key(i)));
        }
        assert_eq!(table.len(), 3);
        let decoded = decode_composite(&columns, &table).unwrap();
        assert_eq!(decoded[0].get(0), Value::varchar("ab"));
        assert_eq!(decoded[0].get(1), Value::Null);
        assert_eq!(decoded[0].get(2), Value::varchar(""));
        assert_eq!(decoded[1].get(2), Value::Int64(3));
    }

    #[test]
    fn string_keys_decode_in_id_order() {
        let mut table = BytesTable::with_slots(8);
        for key in [&b"x"[..], b"", b"yy", b"x"] {
            table.find_or_insert(key, hash_bytes(key));
        }
        let v = decode_strings(&table).unwrap();
        assert_eq!(v.len(), 3);
        assert_eq!(v.get(2), Value::varchar("yy"));
    }
}
