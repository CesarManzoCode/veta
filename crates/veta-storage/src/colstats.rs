//! Typed statistics of a part (contract §5.2, §11.5): rows, NULLs, min/max and the HLL of
//! each column, plus one HLL per primary-key prefix, computed over the column buffers
//! without building a `Value` per row.
//!
//! Persisted hashes are xxh64 (seed 0) of the canonical bytes of each value (§12), the
//! same as [`veta_core::hash::xxh64_value`]. The hash of a key prefix of length 1 is the
//! hash of its value; for length `j > 1` it is xxh64 (seed 0) of the 16 bytes
//! `hash(prefix j−1) LE ‖ hash(column j) LE`. A row with a NULL in the first `j` key
//! columns does not enter the prefix HLL `j`, as NULL does not enter a column HLL.

use std::cmp::Ordering;

use veta_core::hash::xxh64;
use veta_core::hll::Hll;
use veta_core::stats::ColumnStats;
use veta_core::value::cmp_f64;
use veta_core::{Value, Vector, VectorData};

/// Canonical form of a float for hashing: `-0.0` as `0.0`, every NaN as one NaN.
fn canonical_f64(f: f64) -> f64 {
    if f.is_nan() {
        f64::NAN
    } else if f == 0.0 {
        0.0
    } else {
        f
    }
}

fn hash_i64(x: i64) -> u64 {
    xxh64(&x.to_le_bytes(), 0)
}

fn hash_i128(x: i128) -> u64 {
    xxh64(&x.to_le_bytes(), 0)
}

/// Persisted hash of every row of `vector`; rows under NULL get the hash of their
/// placeholder value and must be skipped by the caller.
pub(crate) fn row_hashes(vector: &Vector) -> Vec<u64> {
    match &vector.data {
        VectorData::Boolean(v) => v.iter().map(|&x| hash_i64(x as i64)).collect(),
        VectorData::Int16(v) => v.iter().map(|&x| hash_i64(x as i64)).collect(),
        VectorData::Int32(v) | VectorData::Date(v) => {
            v.iter().map(|&x| hash_i64(x as i64)).collect()
        }
        VectorData::Int64(v) | VectorData::Timestamp(v) => v.iter().map(|&x| hash_i64(x)).collect(),
        VectorData::Decimal64 { values, .. } => values.iter().map(|&x| hash_i64(x)).collect(),
        VectorData::Int128(v) => v.iter().map(|&x| hash_i128(x)).collect(),
        VectorData::Decimal128 { values, .. } => values.iter().map(|&x| hash_i128(x)).collect(),
        VectorData::Float64(v) => v
            .iter()
            .map(|&x| xxh64(&canonical_f64(x).to_bits().to_le_bytes(), 0))
            .collect(),
        VectorData::Varchar { offsets, bytes } => offsets
            .windows(2)
            .map(|w| xxh64(&bytes[w[0] as usize..w[1] as usize], 0))
            .collect(),
    }
}

/// Hash of a key prefix one column longer than the one hashed to `prefix`.
pub(crate) fn extend_prefix_hash(prefix: u64, next: u64) -> u64 {
    let mut buf = [0u8; 16];
    buf[..8].copy_from_slice(&prefix.to_le_bytes());
    buf[8..].copy_from_slice(&next.to_le_bytes());
    xxh64(&buf, 0)
}

/// Minimum and maximum of the valid values of `values` under `cmp`.
fn minmax<T: Copy>(
    values: &[T],
    valid: &dyn Fn(usize) -> bool,
    cmp: impl Fn(&T, &T) -> Ordering,
) -> Option<(T, T)> {
    let mut out: Option<(T, T)> = None;
    for (i, v) in values.iter().enumerate() {
        if !valid(i) {
            continue;
        }
        out = Some(match out {
            None => (*v, *v),
            Some((lo, hi)) => (
                if cmp(v, &lo).is_lt() { *v } else { lo },
                if cmp(v, &hi).is_gt() { *v } else { hi },
            ),
        });
    }
    out
}

/// Minimum and maximum non-NULL values of `vector`, in the order of [`Value`].
fn bounds(vector: &Vector) -> Option<(Value, Value)> {
    let validity = vector.validity.as_ref();
    let valid = |i: usize| validity.is_none_or(|b| b.get(i));
    match &vector.data {
        VectorData::Boolean(v) => {
            minmax(v, &valid, Ord::cmp).map(|(a, b)| (Value::Boolean(a), Value::Boolean(b)))
        }
        VectorData::Int16(v) => {
            minmax(v, &valid, Ord::cmp).map(|(a, b)| (Value::Int16(a), Value::Int16(b)))
        }
        VectorData::Int32(v) => {
            minmax(v, &valid, Ord::cmp).map(|(a, b)| (Value::Int32(a), Value::Int32(b)))
        }
        VectorData::Int64(v) => {
            minmax(v, &valid, Ord::cmp).map(|(a, b)| (Value::Int64(a), Value::Int64(b)))
        }
        VectorData::Int128(v) => {
            minmax(v, &valid, Ord::cmp).map(|(a, b)| (Value::Int128(a), Value::Int128(b)))
        }
        VectorData::Float64(v) => minmax(v, &valid, |a, b| cmp_f64(*a, *b))
            .map(|(a, b)| (Value::Float64(a), Value::Float64(b))),
        VectorData::Date(v) => {
            minmax(v, &valid, Ord::cmp).map(|(a, b)| (Value::Date(a), Value::Date(b)))
        }
        VectorData::Timestamp(v) => {
            minmax(v, &valid, Ord::cmp).map(|(a, b)| (Value::Timestamp(a), Value::Timestamp(b)))
        }
        VectorData::Decimal64 {
            values,
            precision,
            scale,
        } => minmax(values, &valid, Ord::cmp).map(|(a, b)| {
            let dec = |value: i64| Value::Decimal {
                value: value as i128,
                precision: *precision,
                scale: *scale,
            };
            (dec(a), dec(b))
        }),
        VectorData::Decimal128 {
            values,
            precision,
            scale,
        } => minmax(values, &valid, Ord::cmp).map(|(a, b)| {
            let dec = |value: i128| Value::Decimal {
                value,
                precision: *precision,
                scale: *scale,
            };
            (dec(a), dec(b))
        }),
        VectorData::Varchar { offsets, bytes } => {
            let mut out: Option<(&[u8], &[u8])> = None;
            for (i, w) in offsets.windows(2).enumerate() {
                if !valid(i) {
                    continue;
                }
                let s = &bytes[w[0] as usize..w[1] as usize];
                out = Some(match out {
                    None => (s, s),
                    Some((lo, hi)) => (lo.min(s), hi.max(s)),
                });
            }
            out.map(|(a, b)| (Value::Varchar(a.to_vec()), Value::Varchar(b.to_vec())))
        }
    }
}

/// Adds the rows of `vector` to `stats`; `hashes` are its [`row_hashes`].
pub(crate) fn update_column_stats(stats: &mut ColumnStats, vector: &Vector, hashes: &[u64]) {
    debug_assert_eq!(hashes.len(), vector.len());
    stats.rows += vector.len() as u64;
    stats.nulls += vector.null_count() as u64;
    match &vector.validity {
        None => hashes.iter().for_each(|&h| stats.hll.add_hash(h)),
        Some(bitmap) => {
            for (i, &h) in hashes.iter().enumerate() {
                if bitmap.get(i) {
                    stats.hll.add_hash(h);
                }
            }
        }
    }
    if let Some((lo, hi)) = bounds(vector) {
        if stats.min.as_ref().is_none_or(|m| &lo < m) {
            stats.min = Some(lo);
        }
        if stats.max.as_ref().is_none_or(|m| &hi > m) {
            stats.max = Some(hi);
        }
    }
}

/// Adds the rows of the key columns `key` (in key order) to the prefix HLLs `hlls`
/// (`hlls[j]` is the prefix of length `j + 1`).
pub(crate) fn update_prefix_hlls(hlls: &mut [Hll], key: &[&Vector]) {
    debug_assert_eq!(hlls.len(), key.len());
    let Some(first) = key.first() else {
        return;
    };
    let rows = first.len();
    let mut prefix = vec![0u64; rows];
    let mut alive = vec![true; rows];
    for (j, (hll, vector)) in hlls.iter_mut().zip(key).enumerate() {
        let hashes = row_hashes(vector);
        for row in 0..rows {
            if !alive[row] {
                continue;
            }
            if !vector.is_valid(row) {
                alive[row] = false;
                continue;
            }
            prefix[row] = if j == 0 {
                hashes[row]
            } else {
                extend_prefix_hash(prefix[row], hashes[row])
            };
            hll.add_hash(prefix[row]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use veta_core::DataType;
    use veta_core::hash::xxh64_value;

    fn sample_vectors() -> Vec<Vector> {
        let dec = |value: i128, precision: u8, scale: u8| Value::Decimal {
            value,
            precision,
            scale,
        };
        let cases: Vec<(DataType, Vec<Value>)> = vec![
            (
                DataType::Boolean,
                vec![Value::Boolean(true), Value::Null, Value::Boolean(false)],
            ),
            (
                DataType::Int16,
                vec![Value::Int16(-3), Value::Int16(9), Value::Null],
            ),
            (
                DataType::Int32,
                vec![Value::Int32(7), Value::Null, Value::Int32(i32::MIN)],
            ),
            (
                DataType::Int64,
                vec![Value::Int64(1), Value::Int64(-9), Value::Int64(1)],
            ),
            (
                DataType::Float64,
                vec![
                    Value::Float64(-0.0),
                    Value::Float64(f64::NAN),
                    Value::Float64(2.5),
                ],
            ),
            (
                DataType::decimal(15, 2).unwrap(),
                vec![dec(1234, 15, 2), Value::Null, dec(-5, 15, 2)],
            ),
            (
                DataType::decimal(30, 4).unwrap(),
                vec![dec(10i128.pow(30) - 1, 30, 4), dec(1, 30, 4), Value::Null],
            ),
            (
                DataType::Varchar,
                vec![
                    Value::varchar("b"),
                    Value::varchar(""),
                    Value::varchar("ab"),
                ],
            ),
            (
                DataType::Date,
                vec![Value::Date(0), Value::Null, Value::Date(-1)],
            ),
            (
                DataType::Timestamp,
                vec![Value::Timestamp(5), Value::Timestamp(-1), Value::Null],
            ),
            (DataType::Int64, vec![Value::Null, Value::Null, Value::Null]),
        ];
        cases
            .into_iter()
            .map(|(t, values)| Vector::from_values(t, &values).unwrap())
            .collect()
    }

    #[test]
    fn row_hashes_match_the_canonical_value_hash() {
        for vector in sample_vectors() {
            let hashes = row_hashes(&vector);
            for (i, &h) in hashes.iter().enumerate() {
                if let Some(expected) = xxh64_value(&vector.get(i)) {
                    assert_eq!(h, expected, "{} row {i}", vector.data_type());
                }
            }
        }
    }

    #[test]
    fn typed_stats_match_the_value_based_ones() {
        for vector in sample_vectors() {
            let mut typed = ColumnStats::new();
            update_column_stats(&mut typed, &vector, &row_hashes(&vector));
            update_column_stats(&mut typed, &vector, &row_hashes(&vector));
            let mut reference = ColumnStats::new();
            reference.update_vector(&vector);
            reference.update_vector(&vector);
            assert_eq!(typed, reference, "{}", vector.data_type());
        }
    }

    #[test]
    fn prefix_hlls_skip_nulls_and_chain_hashes() {
        let a = Vector::from_values(
            DataType::Int64,
            &[
                Value::Int64(1),
                Value::Int64(1),
                Value::Null,
                Value::Int64(2),
            ],
        )
        .unwrap();
        let b = Vector::from_values(
            DataType::Varchar,
            &[
                Value::varchar("x"),
                Value::varchar("y"),
                Value::varchar("x"),
                Value::Null,
            ],
        )
        .unwrap();
        let mut hlls = vec![Hll::new(), Hll::new()];
        update_prefix_hlls(&mut hlls, &[&a, &b]);
        assert_eq!(hlls[0].estimate().round(), 2.0);
        assert_eq!(hlls[1].estimate().round(), 2.0);
        let mut expected = Hll::new();
        let h1 = xxh64_value(&Value::Int64(1)).unwrap();
        expected.add_hash(extend_prefix_hash(
            h1,
            xxh64_value(&Value::varchar("x")).unwrap(),
        ));
        expected.add_hash(extend_prefix_hash(
            h1,
            xxh64_value(&Value::varchar("y")).unwrap(),
        ));
        assert_eq!(hlls[1], expected);
    }
}
