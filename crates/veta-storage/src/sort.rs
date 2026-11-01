//! Ordering of an inserted batch by the primary key (contract §5.2) with typed comparators:
//! each key column is compared on its buffer, never through a `Value` per row.
//!
//! The order is that of [`veta_core::Value`]: ascending, NULL after every value, floats by
//! [`cmp_f64`] (`-0.0 = 0.0`, NaN last), strings byte by byte. The sort is stable, so rows
//! with equal keys keep their insertion order (the order is not unique, as in ClickHouse).

use std::cmp::Ordering;

use veta_core::value::cmp_f64;
use veta_core::{Bitmap, Chunk, Result, Vector, VectorData, VetaError};

/// One key column viewed as a typed slice.
enum KeySlice<'a> {
    Bool(&'a [bool]),
    I16(&'a [i16]),
    I32(&'a [i32]),
    I64(&'a [i64]),
    I128(&'a [i128]),
    F64(&'a [f64]),
    Str { offsets: &'a [u32], bytes: &'a [u8] },
}

struct SortKey<'a> {
    values: KeySlice<'a>,
    validity: Option<&'a Bitmap>,
}

impl<'a> SortKey<'a> {
    fn new(vector: &'a Vector) -> SortKey<'a> {
        let values = match &vector.data {
            VectorData::Boolean(v) => KeySlice::Bool(v),
            VectorData::Int16(v) => KeySlice::I16(v),
            VectorData::Int32(v) | VectorData::Date(v) => KeySlice::I32(v),
            VectorData::Int64(v) | VectorData::Timestamp(v) => KeySlice::I64(v),
            VectorData::Decimal64 { values, .. } => KeySlice::I64(values),
            VectorData::Int128(v) => KeySlice::I128(v),
            VectorData::Decimal128 { values, .. } => KeySlice::I128(values),
            VectorData::Float64(v) => KeySlice::F64(v),
            VectorData::Varchar { offsets, bytes } => KeySlice::Str { offsets, bytes },
        };
        SortKey {
            values,
            validity: vector.validity.as_ref().filter(|b| !b.all_set()),
        }
    }

    fn cmp(&self, a: usize, b: usize) -> Ordering {
        if let Some(validity) = self.validity {
            match (validity.get(a), validity.get(b)) {
                (true, true) => {}
                (false, false) => return Ordering::Equal,
                (false, true) => return Ordering::Greater,
                (true, false) => return Ordering::Less,
            }
        }
        match &self.values {
            KeySlice::Bool(v) => v[a].cmp(&v[b]),
            KeySlice::I16(v) => v[a].cmp(&v[b]),
            KeySlice::I32(v) => v[a].cmp(&v[b]),
            KeySlice::I64(v) => v[a].cmp(&v[b]),
            KeySlice::I128(v) => v[a].cmp(&v[b]),
            KeySlice::F64(v) => cmp_f64(v[a], v[b]),
            KeySlice::Str { offsets, bytes } => {
                let sa = &bytes[offsets[a] as usize..offsets[a + 1] as usize];
                let sb = &bytes[offsets[b] as usize..offsets[b + 1] as usize];
                sa.cmp(sb)
            }
        }
    }
}

/// Stable permutation of the rows of `chunk` ordered by the columns `key`, in key order.
pub fn sort_permutation(chunk: &Chunk, key: &[usize]) -> Result<Vec<u32>> {
    let rows = u32::try_from(chunk.len)
        .map_err(|_| VetaError::internal("batch has more than u32::MAX rows"))?;
    if let Some(&bad) = key.iter().find(|&&c| c >= chunk.num_columns()) {
        return Err(VetaError::internal(format!(
            "key column {bad} out of range for {} columns",
            chunk.num_columns()
        )));
    }
    let keys: Vec<SortKey<'_>> = key.iter().map(|&c| SortKey::new(chunk.column(c))).collect();
    let mut perm: Vec<u32> = (0..rows).collect();
    if keys.is_empty() {
        return Ok(perm);
    }
    perm.sort_by(|&a, &b| {
        let (a, b) = (a as usize, b as usize);
        for k in &keys {
            let ord = k.cmp(a, b);
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    });
    Ok(perm)
}

/// `chunk` ordered by `key`; `None` if it already was (no copy needed).
pub fn sort_chunk(chunk: &Chunk, key: &[usize]) -> Result<Option<Chunk>> {
    let perm = sort_permutation(chunk, key)?;
    if perm.iter().enumerate().all(|(i, &p)| i == p as usize) {
        return Ok(None);
    }
    Ok(Some(chunk.gather(&perm)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use veta_core::rand::Rng;
    use veta_core::{DataType, Value};

    #[test]
    fn composite_key_with_repeats_matches_value_order_and_is_stable() {
        let mut rng = Rng::new(42);
        let types = [
            DataType::Varchar,
            DataType::Int32,
            DataType::Float64,
            DataType::Int64,
        ];
        let rows: Vec<Vec<Value>> = (0..3000)
            .map(|i| {
                let s = if rng.next_bool(0.1) {
                    Value::Null
                } else {
                    Value::varchar(["b", "a", "ab", ""][rng.below(4) as usize])
                };
                let n = if rng.next_bool(0.1) {
                    Value::Null
                } else {
                    Value::Int32(rng.range_i64(-3, 3) as i32)
                };
                let f = Value::Float64([-0.0, 0.0, 1.5, f64::NAN][rng.below(4) as usize]);
                vec![s, n, f, Value::Int64(i)]
            })
            .collect();
        let chunk = Chunk::from_rows(&types, &rows).unwrap();
        let key = [1, 0, 2];
        let sorted = sort_chunk(&chunk, &key).unwrap().unwrap();
        let mut expected = rows.clone();
        expected.sort_by(|x, y| {
            key.iter()
                .map(|&c| x[c].cmp(&y[c]))
                .find(|o| o.is_ne())
                .unwrap_or(Ordering::Equal)
        });
        for (i, row) in expected.iter().enumerate() {
            let got = sorted.row(i);
            for c in 0..types.len() {
                // Value equality treats -0.0 as 0.0, so compare the original row id too.
                assert_eq!(got[c], row[c], "row {i} column {c}");
            }
            assert_eq!(got[3], row[3], "stability at row {i}");
        }
        assert!(sort_chunk(&sorted, &key).unwrap().is_none());
    }

    #[test]
    fn nulls_sort_last_and_no_key_keeps_insertion_order() {
        let types = [DataType::Int64];
        let rows = vec![
            vec![Value::Null],
            vec![Value::Int64(2)],
            vec![Value::Int64(-1)],
        ];
        let chunk = Chunk::from_rows(&types, &rows).unwrap();
        assert_eq!(sort_permutation(&chunk, &[0]).unwrap(), vec![2, 1, 0]);
        assert_eq!(sort_permutation(&chunk, &[]).unwrap(), vec![0, 1, 2]);
        assert!(sort_chunk(&chunk, &[]).unwrap().is_none());
        assert!(sort_permutation(&chunk, &[1]).is_err());
    }
}
