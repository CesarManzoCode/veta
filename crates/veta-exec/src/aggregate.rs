//! Hash aggregation (contract §12): one table per worker, then 64 radix partitions merged
//! in parallel.
//!
//! Each worker maps its rows to thread-local group ids through a table specialized for the
//! key layout ([`KeyLayout`]) and updates the aggregate states, one flat array per aggregate
//! indexed by group id, so the update loops have no dispatch per row. Afterwards the groups
//! of every table are split by the top bits of their hash and each partition is merged on
//! its own into a new table.
//!
//! `COUNT(DISTINCT)` keeps `(group id, value)` pairs in 64 hash sets per worker, split by the
//! hash of the value. Once the groups are merged, the same set of every worker is combined
//! with the thread-local ids translated to global ones, and each new pair counts for its
//! group. `MIN`/`MAX` ignore `DISTINCT`, which changes nothing for them; `SUM(DISTINCT)` and
//! `AVG(DISTINCT)` are outside §10.3.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use veta_core::hash::{hash_bytes, hash_u64, hash_u128};
use veta_core::memory::Reservation;
use veta_core::types::MAX_DECIMAL64_PRECISION;
use veta_core::value::cmp_f64;
use veta_core::{Bitmap, Chunk, DataType, Result, Vector, VectorData, VetaError, decimal};
use veta_sql::{AggFunc, AggregateExpr, BoundExpr};

use crate::context::{ExecContext, new_reservation};
use crate::group_keys::{
    FixedColumn, KeyBuf, KeyLayout, canonical_bits, decode_composite, decode_strings, pack_keys,
    serialize_keys, unpack_column,
};
use crate::hash_table::{BytesTable, FixedKey, FixedTable, PARTITIONS, partition_of};
use crate::pipeline::{
    Batch, CHUNK_ROWS, Evaluator, Materialized, Pipeline, chunk_bytes, parallel_tasks,
};

/// Slots of a new group table and of a new `COUNT(DISTINCT)` set.
const GROUP_SLOTS: usize = 1024;
const DISTINCT_SLOTS: usize = 16;

fn type_mismatch() -> VetaError {
    VetaError::internal("aggregate input has an unexpected type")
}

#[inline]
fn each_valid(n: usize, validity: Option<&Bitmap>, mut f: impl FnMut(usize)) {
    match validity {
        None => (0..n).for_each(&mut f),
        Some(bitmap) => (0..n).filter(|&i| bitmap.get(i)).for_each(&mut f),
    }
}

fn sum_i64<T: Copy + Into<i64>>(
    sums: &mut [i64],
    seen: &mut [bool],
    gids: &[u32],
    values: &[T],
    validity: Option<&Bitmap>,
) -> Result<()> {
    let mut overflow = false;
    each_valid(gids.len(), validity, |i| {
        let g = gids[i] as usize;
        let (sum, o) = sums[g].overflowing_add(values[i].into());
        sums[g] = sum;
        seen[g] = true;
        overflow |= o;
    });
    if overflow {
        return Err(VetaError::user("integer overflow in SUM"));
    }
    Ok(())
}

fn sum_i128<T: Copy + Into<i128>>(
    sums: &mut [i128],
    seen: &mut [bool],
    gids: &[u32],
    values: &[T],
    validity: Option<&Bitmap>,
) -> Result<()> {
    let mut overflow = false;
    each_valid(gids.len(), validity, |i| {
        let g = gids[i] as usize;
        let (sum, o) = sums[g].overflowing_add(values[i].into());
        sums[g] = sum;
        seen[g] = true;
        overflow |= o;
    });
    if overflow {
        return Err(VetaError::user("integer overflow in SUM"));
    }
    Ok(())
}

fn avg_i128<T: Copy + Into<i128>>(
    sums: &mut [i128],
    counts: &mut [i64],
    gids: &[u32],
    values: &[T],
    validity: Option<&Bitmap>,
) -> Result<()> {
    let mut overflow = false;
    each_valid(gids.len(), validity, |i| {
        let g = gids[i] as usize;
        let (sum, o) = sums[g].overflowing_add(values[i].into());
        sums[g] = sum;
        counts[g] += 1;
        overflow |= o;
    });
    if overflow {
        return Err(VetaError::user("integer overflow in AVG"));
    }
    Ok(())
}

fn min_max<S: Copy + Ord, T: Copy + Into<S>>(
    best: &mut [S],
    seen: &mut [bool],
    max: bool,
    gids: &[u32],
    values: &[T],
    validity: Option<&Bitmap>,
) {
    each_valid(gids.len(), validity, |i| {
        let g = gids[i] as usize;
        let value: S = values[i].into();
        if !seen[g]
            || (if max {
                value > best[g]
            } else {
                value < best[g]
            })
        {
            best[g] = value;
            seen[g] = true;
        }
    });
}

#[inline]
fn better_f64(value: f64, current: f64, max: bool) -> bool {
    let order = cmp_f64(value, current);
    if max {
        order == std::cmp::Ordering::Greater
    } else {
        order == std::cmp::Ordering::Less
    }
}

fn validity_from(seen: &[bool]) -> Option<Bitmap> {
    if seen.iter().all(|&s| s) {
        None
    } else {
        Some(Bitmap::from_bools(seen))
    }
}

/// A vector of `data_type` (any type of at most 64 bits) from the widened `values`.
fn vector_from_i64(
    values: &[i64],
    data_type: DataType,
    validity: Option<Bitmap>,
) -> Result<Vector> {
    let data = match data_type {
        DataType::Boolean => VectorData::Boolean(values.iter().map(|&v| v != 0).collect()),
        DataType::Int16 => VectorData::Int16(values.iter().map(|&v| v as i16).collect()),
        DataType::Int32 => VectorData::Int32(values.iter().map(|&v| v as i32).collect()),
        DataType::Int64 => VectorData::Int64(values.to_vec()),
        DataType::Date => VectorData::Date(values.iter().map(|&v| v as i32).collect()),
        DataType::Timestamp => VectorData::Timestamp(values.to_vec()),
        DataType::Decimal { precision, scale } if precision <= MAX_DECIMAL64_PRECISION => {
            VectorData::Decimal64 {
                values: values.to_vec(),
                precision,
                scale,
            }
        }
        _ => return Err(type_mismatch()),
    };
    Vector::new(data, validity)
}

/// A vector of `data_type` (`Int128` or a wide decimal) from `values`.
fn vector_from_i128(
    values: &[i128],
    data_type: DataType,
    validity: Option<Bitmap>,
) -> Result<Vector> {
    let data = match data_type {
        DataType::Int128 => VectorData::Int128(values.to_vec()),
        DataType::Decimal { precision, scale } if precision > MAX_DECIMAL64_PRECISION => {
            if values
                .iter()
                .any(|&v| !decimal::fits_precision(v, precision))
            {
                return Err(VetaError::user(format!(
                    "decimal overflow: result does not fit DECIMAL({precision},{scale})"
                )));
            }
            VectorData::Decimal128 {
                values: values.to_vec(),
                precision,
                scale,
            }
        }
        _ => return Err(type_mismatch()),
    };
    Vector::new(data, validity)
}

/// State of one aggregate for every group of a table.
#[derive(Debug)]
enum AggState {
    CountStar(Vec<i64>),
    Count(Vec<i64>),
    /// `SUM(Int16|Int32)`.
    SumI64 {
        sums: Vec<i64>,
        seen: Vec<bool>,
    },
    /// `SUM(Int64|Int128|Decimal)`, exact.
    SumI128 {
        sums: Vec<i128>,
        seen: Vec<bool>,
    },
    SumF64 {
        sums: Vec<f64>,
        seen: Vec<bool>,
    },
    /// `AVG(integer|decimal)`: the exact sum and the count (§4).
    AvgI128 {
        sums: Vec<i128>,
        counts: Vec<i64>,
        scale: u8,
    },
    AvgF64 {
        sums: Vec<f64>,
        counts: Vec<i64>,
    },
    /// `MIN`/`MAX` of any type of at most 64 bits, widened.
    MinMaxI64 {
        best: Vec<i64>,
        seen: Vec<bool>,
        max: bool,
    },
    MinMaxI128 {
        best: Vec<i128>,
        seen: Vec<bool>,
        max: bool,
    },
    MinMaxF64 {
        best: Vec<f64>,
        seen: Vec<bool>,
        max: bool,
    },
    MinMaxStr {
        best: Vec<Option<Box<[u8]>>>,
        max: bool,
    },
    /// `COUNT(DISTINCT)`: its counts come from the merged sets.
    Distinct,
}

impl AggState {
    fn new(func: AggFunc, distinct: bool, input: Option<DataType>) -> Result<AggState> {
        let is_max = func == AggFunc::Max;
        Ok(match (func, input) {
            (AggFunc::CountStar, _) => AggState::CountStar(Vec::new()),
            (AggFunc::Count, _) if distinct => AggState::Distinct,
            (AggFunc::Count, _) => AggState::Count(Vec::new()),
            (AggFunc::Sum | AggFunc::Avg, _) if distinct => {
                return Err(VetaError::unsupported(format!(
                    "{}(DISTINCT)",
                    func.name().to_ascii_uppercase()
                )));
            }
            (AggFunc::Sum, Some(DataType::Int16 | DataType::Int32)) => AggState::SumI64 {
                sums: Vec::new(),
                seen: Vec::new(),
            },
            (AggFunc::Sum, Some(DataType::Float64)) => AggState::SumF64 {
                sums: Vec::new(),
                seen: Vec::new(),
            },
            (AggFunc::Sum, Some(DataType::Int64 | DataType::Int128 | DataType::Decimal { .. })) => {
                AggState::SumI128 {
                    sums: Vec::new(),
                    seen: Vec::new(),
                }
            }
            (AggFunc::Avg, Some(DataType::Float64)) => AggState::AvgF64 {
                sums: Vec::new(),
                counts: Vec::new(),
            },
            (AggFunc::Avg, Some(t)) if t.is_numeric() => AggState::AvgI128 {
                sums: Vec::new(),
                counts: Vec::new(),
                scale: match t {
                    DataType::Decimal { scale, .. } => scale,
                    _ => 0,
                },
            },
            (AggFunc::Min | AggFunc::Max, Some(DataType::Varchar)) => AggState::MinMaxStr {
                best: Vec::new(),
                max: is_max,
            },
            (AggFunc::Min | AggFunc::Max, Some(DataType::Float64)) => AggState::MinMaxF64 {
                best: Vec::new(),
                seen: Vec::new(),
                max: is_max,
            },
            (AggFunc::Min | AggFunc::Max, Some(t)) if t.fixed_width() == Some(16) => {
                AggState::MinMaxI128 {
                    best: Vec::new(),
                    seen: Vec::new(),
                    max: is_max,
                }
            }
            (AggFunc::Min | AggFunc::Max, Some(_)) => AggState::MinMaxI64 {
                best: Vec::new(),
                seen: Vec::new(),
                max: is_max,
            },
            _ => return Err(type_mismatch()),
        })
    }

    /// Makes room for `n` groups.
    fn grow(&mut self, n: usize) {
        match self {
            AggState::CountStar(counts) | AggState::Count(counts) => counts.resize(n, 0),
            AggState::SumI64 { sums, seen } => {
                sums.resize(n, 0);
                seen.resize(n, false);
            }
            AggState::SumI128 { sums, seen } => {
                sums.resize(n, 0);
                seen.resize(n, false);
            }
            AggState::SumF64 { sums, seen } => {
                sums.resize(n, 0.0);
                seen.resize(n, false);
            }
            AggState::AvgI128 { sums, counts, .. } => {
                sums.resize(n, 0);
                counts.resize(n, 0);
            }
            AggState::AvgF64 { sums, counts } => {
                sums.resize(n, 0.0);
                counts.resize(n, 0);
            }
            AggState::MinMaxI64 { best, seen, .. } => {
                best.resize(n, 0);
                seen.resize(n, false);
            }
            AggState::MinMaxI128 { best, seen, .. } => {
                best.resize(n, 0);
                seen.resize(n, false);
            }
            AggState::MinMaxF64 { best, seen, .. } => {
                best.resize(n, 0.0);
                seen.resize(n, false);
            }
            AggState::MinMaxStr { best, .. } => best.resize(n, None),
            AggState::Distinct => {}
        }
    }

    /// Adds `arg` (one value per row, `None` only for `COUNT(*)`) to the groups `gids`.
    fn update(&mut self, gids: &[u32], arg: Option<&Vector>) -> Result<()> {
        if let AggState::CountStar(counts) = self {
            for &g in gids {
                counts[g as usize] += 1;
            }
            return Ok(());
        }
        if matches!(self, AggState::Distinct) {
            return Ok(());
        }
        let v = arg.ok_or_else(type_mismatch)?;
        let validity = v.validity.as_ref();
        match self {
            AggState::CountStar(_) | AggState::Distinct => {}
            AggState::Count(counts) => each_valid(gids.len(), validity, |i| {
                counts[gids[i] as usize] += 1;
            }),
            AggState::SumI64 { sums, seen } => match &v.data {
                VectorData::Int16(x) => sum_i64(sums, seen, gids, x, validity)?,
                VectorData::Int32(x) => sum_i64(sums, seen, gids, x, validity)?,
                _ => return Err(type_mismatch()),
            },
            AggState::SumI128 { sums, seen } => match &v.data {
                VectorData::Int64(x) => sum_i128(sums, seen, gids, x, validity)?,
                VectorData::Int128(x) => sum_i128(sums, seen, gids, x, validity)?,
                VectorData::Decimal64 { values, .. } => {
                    sum_i128(sums, seen, gids, values, validity)?
                }
                VectorData::Decimal128 { values, .. } => {
                    sum_i128(sums, seen, gids, values, validity)?
                }
                _ => return Err(type_mismatch()),
            },
            AggState::SumF64 { sums, seen } => {
                let VectorData::Float64(x) = &v.data else {
                    return Err(type_mismatch());
                };
                each_valid(gids.len(), validity, |i| {
                    let g = gids[i] as usize;
                    sums[g] += x[i];
                    seen[g] = true;
                });
            }
            AggState::AvgI128 { sums, counts, .. } => match &v.data {
                VectorData::Int16(x) => avg_i128(sums, counts, gids, x, validity)?,
                VectorData::Int32(x) => avg_i128(sums, counts, gids, x, validity)?,
                VectorData::Int64(x) => avg_i128(sums, counts, gids, x, validity)?,
                VectorData::Int128(x) => avg_i128(sums, counts, gids, x, validity)?,
                VectorData::Decimal64 { values, .. } => {
                    avg_i128(sums, counts, gids, values, validity)?
                }
                VectorData::Decimal128 { values, .. } => {
                    avg_i128(sums, counts, gids, values, validity)?
                }
                _ => return Err(type_mismatch()),
            },
            AggState::AvgF64 { sums, counts } => {
                let VectorData::Float64(x) = &v.data else {
                    return Err(type_mismatch());
                };
                each_valid(gids.len(), validity, |i| {
                    let g = gids[i] as usize;
                    sums[g] += x[i];
                    counts[g] += 1;
                });
            }
            AggState::MinMaxI64 { best, seen, max } => {
                let max = *max;
                match &v.data {
                    VectorData::Boolean(x) => min_max(best, seen, max, gids, x, validity),
                    VectorData::Int16(x) => min_max(best, seen, max, gids, x, validity),
                    VectorData::Int32(x) | VectorData::Date(x) => {
                        min_max(best, seen, max, gids, x, validity)
                    }
                    VectorData::Int64(x) | VectorData::Timestamp(x) => {
                        min_max(best, seen, max, gids, x, validity)
                    }
                    VectorData::Decimal64 { values, .. } => {
                        min_max(best, seen, max, gids, values, validity)
                    }
                    _ => return Err(type_mismatch()),
                }
            }
            AggState::MinMaxI128 { best, seen, max } => {
                let max = *max;
                match &v.data {
                    VectorData::Int128(x) => min_max(best, seen, max, gids, x, validity),
                    VectorData::Decimal128 { values, .. } => {
                        min_max(best, seen, max, gids, values, validity)
                    }
                    _ => return Err(type_mismatch()),
                }
            }
            AggState::MinMaxF64 { best, seen, max } => {
                let max = *max;
                let VectorData::Float64(x) = &v.data else {
                    return Err(type_mismatch());
                };
                each_valid(gids.len(), validity, |i| {
                    let g = gids[i] as usize;
                    if !seen[g] || better_f64(x[i], best[g], max) {
                        best[g] = x[i];
                        seen[g] = true;
                    }
                });
            }
            AggState::MinMaxStr { best, max } => {
                let max = *max;
                let VectorData::Varchar { offsets, bytes } = &v.data else {
                    return Err(type_mismatch());
                };
                each_valid(gids.len(), validity, |i| {
                    let slot = &mut best[gids[i] as usize];
                    let value = &bytes[offsets[i] as usize..offsets[i + 1] as usize];
                    let better = match slot {
                        None => true,
                        Some(current) => {
                            if max {
                                value > &**current
                            } else {
                                value < &**current
                            }
                        }
                    };
                    if better {
                        *slot = Some(value.into());
                    }
                });
            }
        }
        Ok(())
    }

    /// Folds group `src` of `other` into group `dst` of this state.
    fn merge_group(&mut self, dst: usize, other: &AggState, src: usize) -> Result<()> {
        match (self, other) {
            (AggState::CountStar(a), AggState::CountStar(b))
            | (AggState::Count(a), AggState::Count(b)) => a[dst] += b[src],
            (AggState::SumI64 { sums: a, seen: sa }, AggState::SumI64 { sums: b, seen: sb }) => {
                if sb[src] {
                    a[dst] = a[dst]
                        .checked_add(b[src])
                        .ok_or_else(|| VetaError::user("integer overflow in SUM"))?;
                    sa[dst] = true;
                }
            }
            (AggState::SumI128 { sums: a, seen: sa }, AggState::SumI128 { sums: b, seen: sb }) => {
                if sb[src] {
                    a[dst] = a[dst]
                        .checked_add(b[src])
                        .ok_or_else(|| VetaError::user("integer overflow in SUM"))?;
                    sa[dst] = true;
                }
            }
            (AggState::SumF64 { sums: a, seen: sa }, AggState::SumF64 { sums: b, seen: sb }) => {
                if sb[src] {
                    a[dst] += b[src];
                    sa[dst] = true;
                }
            }
            (
                AggState::AvgI128 {
                    sums: a,
                    counts: ca,
                    ..
                },
                AggState::AvgI128 {
                    sums: b,
                    counts: cb,
                    ..
                },
            ) => {
                a[dst] = a[dst]
                    .checked_add(b[src])
                    .ok_or_else(|| VetaError::user("integer overflow in AVG"))?;
                ca[dst] += cb[src];
            }
            (
                AggState::AvgF64 {
                    sums: a,
                    counts: ca,
                },
                AggState::AvgF64 {
                    sums: b,
                    counts: cb,
                },
            ) => {
                a[dst] += b[src];
                ca[dst] += cb[src];
            }
            (
                AggState::MinMaxI64 {
                    best: a,
                    seen: sa,
                    max,
                },
                AggState::MinMaxI64 {
                    best: b, seen: sb, ..
                },
            ) => {
                if sb[src]
                    && (!sa[dst]
                        || (if *max {
                            b[src] > a[dst]
                        } else {
                            b[src] < a[dst]
                        }))
                {
                    a[dst] = b[src];
                    sa[dst] = true;
                }
            }
            (
                AggState::MinMaxI128 {
                    best: a,
                    seen: sa,
                    max,
                },
                AggState::MinMaxI128 {
                    best: b, seen: sb, ..
                },
            ) => {
                if sb[src]
                    && (!sa[dst]
                        || (if *max {
                            b[src] > a[dst]
                        } else {
                            b[src] < a[dst]
                        }))
                {
                    a[dst] = b[src];
                    sa[dst] = true;
                }
            }
            (
                AggState::MinMaxF64 {
                    best: a,
                    seen: sa,
                    max,
                },
                AggState::MinMaxF64 {
                    best: b, seen: sb, ..
                },
            ) => {
                if sb[src] && (!sa[dst] || better_f64(b[src], a[dst], *max)) {
                    a[dst] = b[src];
                    sa[dst] = true;
                }
            }
            (AggState::MinMaxStr { best: a, max }, AggState::MinMaxStr { best: b, .. }) => {
                if let Some(value) = &b[src] {
                    let better = match &a[dst] {
                        None => true,
                        Some(current) => {
                            if *max {
                                value > current
                            } else {
                                value < current
                            }
                        }
                    };
                    if better {
                        a[dst] = Some(value.clone());
                    }
                }
            }
            (AggState::Distinct, AggState::Distinct) => {}
            _ => return Err(VetaError::internal("merging different aggregate states")),
        }
        Ok(())
    }

    /// The result column of all the groups. `counts` holds the count of each group for a
    /// `COUNT(DISTINCT)`.
    fn finish(&self, output: DataType, counts: Option<&[AtomicI64]>) -> Result<Vector> {
        match self {
            AggState::CountStar(c) | AggState::Count(c) => {
                Ok(Vector::from_data(VectorData::Int64(c.clone())))
            }
            AggState::SumI64 { sums, seen } => {
                Vector::new(VectorData::Int64(sums.clone()), validity_from(seen))
            }
            AggState::SumI128 { sums, seen } => vector_from_i128(sums, output, validity_from(seen)),
            AggState::SumF64 { sums, seen } => {
                Vector::new(VectorData::Float64(sums.clone()), validity_from(seen))
            }
            AggState::AvgI128 {
                sums,
                counts,
                scale,
            } => {
                let values = sums
                    .iter()
                    .zip(counts)
                    .map(|(&s, &c)| {
                        if c == 0 {
                            0.0
                        } else {
                            decimal::to_f64(s, *scale) / c as f64
                        }
                    })
                    .collect();
                let seen: Vec<bool> = counts.iter().map(|&c| c > 0).collect();
                Vector::new(VectorData::Float64(values), validity_from(&seen))
            }
            AggState::AvgF64 { sums, counts } => {
                let values = sums
                    .iter()
                    .zip(counts)
                    .map(|(&s, &c)| if c == 0 { 0.0 } else { s / c as f64 })
                    .collect();
                let seen: Vec<bool> = counts.iter().map(|&c| c > 0).collect();
                Vector::new(VectorData::Float64(values), validity_from(&seen))
            }
            AggState::MinMaxI64 { best, seen, .. } => {
                vector_from_i64(best, output, validity_from(seen))
            }
            AggState::MinMaxI128 { best, seen, .. } => {
                vector_from_i128(best, output, validity_from(seen))
            }
            AggState::MinMaxF64 { best, seen, .. } => {
                Vector::new(VectorData::Float64(best.clone()), validity_from(seen))
            }
            AggState::MinMaxStr { best, .. } => {
                let mut offsets = Vec::with_capacity(best.len() + 1);
                let mut bytes = Vec::new();
                offsets.push(0u32);
                for value in best {
                    if let Some(value) = value {
                        bytes.extend_from_slice(value);
                    }
                    offsets.push(
                        u32::try_from(bytes.len())
                            .map_err(|_| VetaError::internal("string vector exceeds 4 GiB"))?,
                    );
                }
                let seen: Vec<bool> = best.iter().map(Option::is_some).collect();
                Vector::new(VectorData::Varchar { offsets, bytes }, validity_from(&seen))
            }
            AggState::Distinct => {
                let counts = counts.ok_or_else(type_mismatch)?;
                Ok(Vector::from_data(VectorData::Int64(
                    counts.iter().map(|c| c.load(Ordering::Relaxed)).collect(),
                )))
            }
        }
    }

    /// Bytes held by the state.
    fn bytes(&self) -> u64 {
        let total = match self {
            AggState::CountStar(c) | AggState::Count(c) => c.capacity() * 8,
            AggState::SumI64 { sums, seen } => sums.capacity() * 8 + seen.capacity(),
            AggState::SumI128 { sums, seen } => sums.capacity() * 16 + seen.capacity(),
            AggState::SumF64 { sums, seen } => sums.capacity() * 8 + seen.capacity(),
            AggState::AvgI128 { sums, counts, .. } => sums.capacity() * 16 + counts.capacity() * 8,
            AggState::AvgF64 { sums, counts } => sums.capacity() * 8 + counts.capacity() * 8,
            AggState::MinMaxI64 { best, seen, .. } => best.capacity() * 8 + seen.capacity(),
            AggState::MinMaxI128 { best, seen, .. } => best.capacity() * 16 + seen.capacity(),
            AggState::MinMaxF64 { best, seen, .. } => best.capacity() * 8 + seen.capacity(),
            AggState::MinMaxStr { best, .. } => {
                best.capacity() * 16
                    + best
                        .iter()
                        .flatten()
                        .map(|value| value.len())
                        .sum::<usize>()
            }
            AggState::Distinct => 0,
        };
        total as u64
    }
}

/// The 64 sets of `(group id, value)` of a `COUNT(DISTINCT)`, split by the hash of the
/// value. Values of at most 64 bits go packed, under the group id plus one; the others as
/// the bytes of the group id followed by the bytes of the value.
#[derive(Debug)]
enum DistinctSets {
    Fixed(Vec<FixedTable<u128>>),
    Bytes(Vec<BytesTable>),
}

fn distinct_fixed<T: Copy>(
    sets: &mut [FixedTable<u128>],
    gids: &[u32],
    values: &[T],
    validity: Option<&Bitmap>,
    f: impl Fn(T) -> u64,
) {
    each_valid(gids.len(), validity, |i| {
        let value = f(values[i]);
        let key = ((gids[i] as u128 + 1) << 64) | value as u128;
        sets[partition_of(hash_u64(value))].find_or_insert(key);
    });
}

/// Writes the entry of `value` in the group `group` to `scratch`; returns the hash of the
/// value, which picks the set, and the hash of the entry.
fn distinct_entry(group: u64, value: &[u8], scratch: &mut Vec<u8>) -> (u64, u64) {
    let value_hash = hash_bytes(value);
    scratch.clear();
    scratch.extend_from_slice(&group.to_le_bytes());
    scratch.extend_from_slice(value);
    (
        value_hash,
        hash_u128(((group as u128) << 64) | value_hash as u128),
    )
}

/// Inserts `value` of the group `group` in its set.
fn distinct_bytes_insert(sets: &mut [BytesTable], group: u64, value: &[u8], scratch: &mut Vec<u8>) {
    let (value_hash, hash) = distinct_entry(group, value, scratch);
    sets[partition_of(value_hash)].find_or_insert(scratch, hash);
}

impl DistinctSets {
    fn new(input: DataType) -> DistinctSets {
        if input.fixed_width().is_some_and(|width| width <= 8) {
            DistinctSets::Fixed(
                (0..PARTITIONS)
                    .map(|_| FixedTable::with_slots(DISTINCT_SLOTS))
                    .collect(),
            )
        } else {
            DistinctSets::Bytes(
                (0..PARTITIONS)
                    .map(|_| BytesTable::with_slots(DISTINCT_SLOTS))
                    .collect(),
            )
        }
    }

    fn update(&mut self, gids: &[u32], v: &Vector, scratch: &mut Vec<u8>) -> Result<()> {
        let validity = v.validity.as_ref();
        match (self, &v.data) {
            (DistinctSets::Fixed(sets), VectorData::Boolean(x)) => {
                distinct_fixed(sets, gids, x, validity, |a: bool| a as u64)
            }
            (DistinctSets::Fixed(sets), VectorData::Int16(x)) => {
                distinct_fixed(sets, gids, x, validity, |a: i16| a as u16 as u64)
            }
            (DistinctSets::Fixed(sets), VectorData::Int32(x) | VectorData::Date(x)) => {
                distinct_fixed(sets, gids, x, validity, |a: i32| a as u32 as u64)
            }
            (DistinctSets::Fixed(sets), VectorData::Int64(x) | VectorData::Timestamp(x)) => {
                distinct_fixed(sets, gids, x, validity, |a: i64| a as u64)
            }
            (DistinctSets::Fixed(sets), VectorData::Decimal64 { values, .. }) => {
                distinct_fixed(sets, gids, values, validity, |a: i64| a as u64)
            }
            (DistinctSets::Fixed(sets), VectorData::Float64(x)) => {
                distinct_fixed(sets, gids, x, validity, canonical_bits)
            }
            (DistinctSets::Bytes(sets), VectorData::Varchar { offsets, bytes }) => {
                each_valid(gids.len(), validity, |i| {
                    let value = &bytes[offsets[i] as usize..offsets[i + 1] as usize];
                    distinct_bytes_insert(sets, gids[i] as u64, value, scratch);
                });
            }
            (DistinctSets::Bytes(sets), VectorData::Int128(x)) => {
                each_valid(gids.len(), validity, |i| {
                    distinct_bytes_insert(sets, gids[i] as u64, &x[i].to_le_bytes(), scratch);
                });
            }
            (DistinctSets::Bytes(sets), VectorData::Decimal128 { values, .. }) => {
                each_valid(gids.len(), validity, |i| {
                    distinct_bytes_insert(sets, gids[i] as u64, &values[i].to_le_bytes(), scratch);
                });
            }
            _ => return Err(type_mismatch()),
        }
        Ok(())
    }

    fn bytes(&self) -> u64 {
        match self {
            DistinctSets::Fixed(sets) => sets.iter().map(FixedTable::bytes).sum(),
            DistinctSets::Bytes(sets) => sets.iter().map(BytesTable::bytes).sum(),
        }
    }
}

/// Maps keys to group ids; [`GroupTable::Global`] has one group, always present.
#[derive(Debug)]
enum GroupTable {
    Global,
    Fixed64(FixedTable<u64>),
    Fixed128(FixedTable<u128>),
    Bytes(BytesTable),
}

impl GroupTable {
    fn new(layout: &KeyLayout) -> GroupTable {
        match layout {
            KeyLayout::Global => GroupTable::Global,
            KeyLayout::Fixed64(_) => GroupTable::Fixed64(FixedTable::with_slots(GROUP_SLOTS)),
            KeyLayout::Fixed128(_) => GroupTable::Fixed128(FixedTable::with_slots(GROUP_SLOTS)),
            KeyLayout::Str | KeyLayout::Composite(_) => {
                GroupTable::Bytes(BytesTable::with_slots(GROUP_SLOTS))
            }
        }
    }

    fn len(&self) -> usize {
        match self {
            GroupTable::Global => 1,
            GroupTable::Fixed64(t) => t.len(),
            GroupTable::Fixed128(t) => t.len(),
            GroupTable::Bytes(t) => t.len(),
        }
    }

    fn bytes(&self) -> u64 {
        match self {
            GroupTable::Global => 0,
            GroupTable::Fixed64(t) => t.bytes(),
            GroupTable::Fixed128(t) => t.bytes(),
            GroupTable::Bytes(t) => t.bytes(),
        }
    }
}

/// One aggregate of the query.
#[derive(Debug)]
struct AggSpec {
    func: AggFunc,
    distinct: bool,
    input: Option<DataType>,
    output: DataType,
    arg: Option<Evaluator>,
    /// Index of its sets among the `COUNT(DISTINCT)`s.
    distinct_slot: Option<usize>,
}

/// What the workers share.
#[derive(Debug)]
struct AggPlan {
    groups: Vec<Evaluator>,
    layout: KeyLayout,
    aggregates: Vec<AggSpec>,
    distinct_count: usize,
}

impl AggPlan {
    fn new(groups: &[BoundExpr], aggregates: &[AggregateExpr]) -> Result<AggPlan> {
        let columns: Vec<(DataType, bool)> =
            groups.iter().map(|g| (g.data_type, g.nullable)).collect();
        let mut distinct_count = 0;
        let mut specs = Vec::with_capacity(aggregates.len());
        for agg in aggregates {
            let input = agg.arg.as_ref().map(|a| a.data_type);
            // A trial state rejects what the executor cannot compute before any row is read.
            AggState::new(agg.func, agg.distinct, input)?;
            let distinct_slot = if agg.func == AggFunc::Count && agg.distinct {
                distinct_count += 1;
                Some(distinct_count - 1)
            } else {
                None
            };
            specs.push(AggSpec {
                func: agg.func,
                distinct: agg.distinct,
                input,
                output: agg.data_type,
                arg: agg.arg.as_ref().map(Evaluator::new).transpose()?,
                distinct_slot,
            });
        }
        Ok(AggPlan {
            groups: groups.iter().map(Evaluator::new).collect::<Result<_>>()?,
            layout: KeyLayout::new(&columns),
            aggregates: specs,
            distinct_count,
        })
    }

    fn new_states(&self, groups: usize) -> Result<Vec<AggState>> {
        self.aggregates
            .iter()
            .map(|spec| -> Result<AggState> {
                let mut state = AggState::new(spec.func, spec.distinct, spec.input)?;
                state.grow(groups);
                Ok(state)
            })
            .collect()
    }

    fn initial_groups(&self) -> usize {
        usize::from(matches!(self.layout, KeyLayout::Global))
    }
}

/// The table and the states of one worker.
#[derive(Debug)]
struct ThreadAgg {
    table: GroupTable,
    states: Vec<AggState>,
    distinct: Vec<DistinctSets>,
    groups: usize,
    gids: Vec<u32>,
    keys64: Vec<u64>,
    keys128: Vec<u128>,
    key_buf: KeyBuf,
    scratch: Vec<u8>,
    mem: Reservation<'static>,
}

impl ThreadAgg {
    fn new(plan: &AggPlan) -> Result<ThreadAgg> {
        let groups = plan.initial_groups();
        let mut distinct = Vec::with_capacity(plan.distinct_count);
        for spec in &plan.aggregates {
            if spec.distinct_slot.is_some() {
                let input = spec.input.ok_or_else(type_mismatch)?;
                distinct.push(DistinctSets::new(input));
            }
        }
        Ok(ThreadAgg {
            table: GroupTable::new(&plan.layout),
            states: plan.new_states(groups)?,
            distinct,
            groups,
            gids: Vec::new(),
            keys64: Vec::new(),
            keys128: Vec::new(),
            key_buf: KeyBuf::default(),
            scratch: Vec::new(),
            mem: new_reservation()?,
        })
    }

    fn consume(&mut self, plan: &AggPlan, batch: &Batch) -> Result<()> {
        let n = batch.rows();
        let vectors = plan
            .groups
            .iter()
            .map(|g| g.vector(batch))
            .collect::<Result<Vec<_>>>()?;
        self.gids.clear();
        match (&mut self.table, &plan.layout) {
            (GroupTable::Global, KeyLayout::Global) => self.gids.resize(n, 0),
            (GroupTable::Fixed64(table), KeyLayout::Fixed64(columns)) => {
                pack_keys(columns, &vectors, n, &mut self.keys64)?;
                self.gids
                    .extend(self.keys64.iter().map(|&k| table.find_or_insert(k)));
            }
            (GroupTable::Fixed128(table), KeyLayout::Fixed128(columns)) => {
                pack_keys(columns, &vectors, n, &mut self.keys128)?;
                self.gids
                    .extend(self.keys128.iter().map(|&k| table.find_or_insert(k)));
            }
            (GroupTable::Bytes(table), KeyLayout::Str) => {
                let v = vectors.first().ok_or_else(type_mismatch)?;
                let VectorData::Varchar { offsets, bytes } = &v.data else {
                    return Err(type_mismatch());
                };
                if v.null_count() > 0 {
                    return Err(VetaError::internal("NULL in a NOT NULL group column"));
                }
                for i in 0..n {
                    let key = &bytes[offsets[i] as usize..offsets[i + 1] as usize];
                    self.gids.push(table.find_or_insert(key, hash_bytes(key)));
                }
            }
            (GroupTable::Bytes(table), KeyLayout::Composite(columns)) => {
                serialize_keys(columns, &vectors, n, &mut self.key_buf)?;
                for i in 0..n {
                    let key = self.key_buf.key(i);
                    self.gids.push(table.find_or_insert(key, hash_bytes(key)));
                }
            }
            _ => return Err(VetaError::internal("group table does not match its layout")),
        }
        let groups = self.table.len();
        if groups > self.groups {
            for state in &mut self.states {
                state.grow(groups);
            }
            self.groups = groups;
        }
        for (state, spec) in self.states.iter_mut().zip(&plan.aggregates) {
            let arg = match &spec.arg {
                Some(arg) => Some(arg.vector(batch)?),
                None => None,
            };
            let arg = arg.as_deref();
            state.update(&self.gids, arg)?;
            if let Some(slot) = spec.distinct_slot {
                let v = arg.ok_or_else(type_mismatch)?;
                self.distinct[slot].update(&self.gids, v, &mut self.scratch)?;
            }
        }
        let bytes = self.bytes();
        self.mem.resize(bytes)
    }

    fn bytes(&self) -> u64 {
        self.table.bytes()
            + self.states.iter().map(AggState::bytes).sum::<u64>()
            + self.distinct.iter().map(DistinctSets::bytes).sum::<u64>()
            + self.gids.capacity() as u64 * 4
    }
}

/// The groups of one table split by radix partition.
#[derive(Debug)]
enum PartitionLists {
    Global,
    Fixed64(Vec<Vec<(u64, u32)>>),
    Fixed128(Vec<Vec<(u128, u32)>>),
    /// Group ids; their keys and hashes stay in the table.
    Bytes(Vec<Vec<u32>>),
}

fn fixed_lists<K: FixedKey>(table: &FixedTable<K>) -> Vec<Vec<(K, u32)>> {
    let mut lists = vec![Vec::new(); PARTITIONS];
    for (key, id) in table.entries() {
        lists[partition_of(key.hash64())].push((key, id));
    }
    lists
}

fn partition_groups(table: &GroupTable) -> PartitionLists {
    match table {
        GroupTable::Global => PartitionLists::Global,
        GroupTable::Fixed64(t) => PartitionLists::Fixed64(fixed_lists(t)),
        GroupTable::Fixed128(t) => PartitionLists::Fixed128(fixed_lists(t)),
        GroupTable::Bytes(t) => {
            let mut lists = vec![Vec::new(); PARTITIONS];
            for id in 0..t.len() {
                lists[partition_of(t.hash(id))].push(id as u32);
            }
            PartitionLists::Bytes(lists)
        }
    }
}

/// The merged table and states of one radix partition.
#[derive(Debug)]
struct MergedPartition {
    table: GroupTable,
    states: Vec<AggState>,
    mem: Reservation<'static>,
}

/// Where group `src` of a worker ended up: partition in the high half, group in the low one.
fn store_translation(translation: Option<&[AtomicU64]>, src: usize, partition: usize, group: u32) {
    if let Some(translation) = translation {
        translation[src].store(((partition as u64) << 32) | group as u64, Ordering::Relaxed);
    }
}

fn merge_fixed<K: FixedKey>(
    table: &mut FixedTable<K>,
    entries: &[(K, u32)],
    states: &mut [AggState],
    source: &[AggState],
    translation: Option<&[AtomicU64]>,
    partition: usize,
) -> Result<()> {
    let mut groups = table.len();
    for &(key, src) in entries {
        let group = table.find_or_insert(key);
        if table.len() > groups {
            groups = table.len();
            for state in states.iter_mut() {
                state.grow(groups);
            }
        }
        for (state, from) in states.iter_mut().zip(source) {
            state.merge_group(group as usize, from, src as usize)?;
        }
        store_translation(translation, src as usize, partition, group);
    }
    Ok(())
}

fn merge_bytes(
    table: &mut BytesTable,
    entries: &[u32],
    source_table: &BytesTable,
    states: &mut [AggState],
    source: &[AggState],
    translation: Option<&[AtomicU64]>,
    partition: usize,
) -> Result<()> {
    let mut groups = table.len();
    for &src in entries {
        let id = src as usize;
        let group = table.find_or_insert(source_table.key(id), source_table.hash(id));
        if table.len() > groups {
            groups = table.len();
            for state in states.iter_mut() {
                state.grow(groups);
            }
        }
        for (state, from) in states.iter_mut().zip(source) {
            state.merge_group(group as usize, from, id)?;
        }
        store_translation(translation, id, partition, group);
    }
    Ok(())
}

/// Merges partition `p` of every worker.
fn merge_partition(
    plan: &AggPlan,
    p: usize,
    threads: &[ThreadAgg],
    lists: &[PartitionLists],
    translations: &[Vec<AtomicU64>],
) -> Result<MergedPartition> {
    let mut merged = MergedPartition {
        table: GroupTable::new(&plan.layout),
        states: plan.new_states(plan.initial_groups())?,
        mem: new_reservation()?,
    };
    for (t, thread) in threads.iter().enumerate() {
        let translation = (plan.distinct_count > 0).then(|| translations[t].as_slice());
        match (&lists[t], &mut merged.table, &thread.table) {
            (PartitionLists::Global, GroupTable::Global, _) => {
                for (state, from) in merged.states.iter_mut().zip(&thread.states) {
                    state.merge_group(0, from, 0)?;
                }
                store_translation(translation, 0, p, 0);
            }
            (PartitionLists::Fixed64(l), GroupTable::Fixed64(table), _) => merge_fixed(
                table,
                &l[p],
                &mut merged.states,
                &thread.states,
                translation,
                p,
            )?,
            (PartitionLists::Fixed128(l), GroupTable::Fixed128(table), _) => merge_fixed(
                table,
                &l[p],
                &mut merged.states,
                &thread.states,
                translation,
                p,
            )?,
            (PartitionLists::Bytes(l), GroupTable::Bytes(table), GroupTable::Bytes(source)) => {
                merge_bytes(
                    table,
                    &l[p],
                    source,
                    &mut merged.states,
                    &thread.states,
                    translation,
                    p,
                )?
            }
            _ => return Err(VetaError::internal("workers have different group tables")),
        }
        let bytes = merged.table.bytes() + merged.states.iter().map(AggState::bytes).sum::<u64>();
        merged.mem.resize(bytes)?;
    }
    Ok(merged)
}

/// Combines set `q` of the `COUNT(DISTINCT)` number `slot` of every worker; each pair seen
/// for the first time adds one to the count of its global group.
fn merge_distinct(
    q: usize,
    slot: usize,
    threads: &[ThreadAgg],
    translations: &[Vec<AtomicU64>],
    bases: &[usize],
    counts: &[AtomicI64],
) -> Result<()> {
    let global_of = |t: usize, src: usize| -> usize {
        let place = translations[t][src].load(Ordering::Relaxed);
        bases[(place >> 32) as usize] + (place & 0xFFFF_FFFF) as usize
    };
    let mut mem = new_reservation()?;
    let mut fixed = FixedTable::<u128>::with_slots(DISTINCT_SLOTS);
    let mut bytes = BytesTable::with_slots(DISTINCT_SLOTS);
    let mut scratch = Vec::new();
    for (t, thread) in threads.iter().enumerate() {
        match &thread.distinct[slot] {
            DistinctSets::Fixed(sets) => {
                for (key, _) in sets[q].entries() {
                    let src = (key >> 64) as usize - 1;
                    let global = global_of(t, src);
                    let new_key = ((global as u128 + 1) << 64) | (key as u64) as u128;
                    let before = fixed.len();
                    fixed.find_or_insert(new_key);
                    if fixed.len() > before {
                        counts[global].fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            DistinctSets::Bytes(sets) => {
                for id in 0..sets[q].len() {
                    let key = sets[q].key(id);
                    let (group, value) = key.split_at(8);
                    let src = u64::from_le_bytes(group.try_into().expect("8 bytes")) as usize;
                    let global = global_of(t, src);
                    let (_, hash) = distinct_entry(global as u64, value, &mut scratch);
                    let before = bytes.len();
                    bytes.find_or_insert(&scratch, hash);
                    if bytes.len() > before {
                        counts[global].fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }
        mem.resize(fixed.bytes() + bytes.bytes())?;
    }
    Ok(())
}

fn decode_fixed<K: FixedKey>(
    columns: &[FixedColumn],
    table: &FixedTable<K>,
) -> Result<Vec<Vector>> {
    let mut keys = vec![K::default(); table.len()];
    for (key, id) in table.entries() {
        keys[id as usize] = key;
    }
    columns.iter().map(|c| unpack_column(&keys, c)).collect()
}

/// The output of one merged partition: its groups, then its aggregates.
fn finish_partition(
    plan: &AggPlan,
    merged: &MergedPartition,
    counts: &[Vec<AtomicI64>],
    base: usize,
) -> Result<Option<Chunk>> {
    let n = merged.table.len();
    if n == 0 {
        return Ok(None);
    }
    let mut columns = match (&plan.layout, &merged.table) {
        (KeyLayout::Global, GroupTable::Global) => Vec::new(),
        (KeyLayout::Fixed64(c), GroupTable::Fixed64(t)) => decode_fixed(c, t)?,
        (KeyLayout::Fixed128(c), GroupTable::Fixed128(t)) => decode_fixed(c, t)?,
        (KeyLayout::Str, GroupTable::Bytes(t)) => vec![decode_strings(t)?],
        (KeyLayout::Composite(c), GroupTable::Bytes(t)) => decode_composite(c, t)?,
        _ => return Err(VetaError::internal("group table does not match its layout")),
    };
    for (state, spec) in merged.states.iter().zip(&plan.aggregates) {
        let count = spec.distinct_slot.map(|slot| &counts[slot][base..base + n]);
        columns.push(state.finish(spec.output, count)?);
    }
    Chunk::with_len(columns, n).map(Some)
}

/// Runs the aggregation sink over `pipeline`. Output: groups, then aggregates, in chunks
/// of up to [`CHUNK_ROWS`] rows.
pub(crate) fn run(
    pipeline: Pipeline,
    groups: &[BoundExpr],
    aggregates: &[AggregateExpr],
    ctx: &ExecContext<'_>,
) -> Result<Materialized> {
    let plan = AggPlan::new(groups, aggregates)?;
    let threads = pipeline.run_workers(
        ctx,
        || ThreadAgg::new(&plan),
        |state, _m, batch| state.consume(&plan, &batch),
    )?;
    drop(pipeline);

    let partitions = if matches!(plan.layout, KeyLayout::Global) {
        1
    } else {
        PARTITIONS
    };
    let lists = parallel_tasks(ctx.threads, threads.len(), |t| {
        Ok(partition_groups(&threads[t].table))
    })?;
    let mut lists_mem = new_reservation()?;
    let group_rows: usize = threads.iter().map(|t| t.table.len()).sum();
    lists_mem.grow(group_rows as u64 * 24)?;
    let translations: Vec<Vec<AtomicU64>> = threads
        .iter()
        .map(|t| {
            let groups = if plan.distinct_count > 0 {
                t.table.len()
            } else {
                0
            };
            (0..groups).map(|_| AtomicU64::new(0)).collect()
        })
        .collect();
    let merged = parallel_tasks(ctx.threads, partitions, |p| {
        merge_partition(&plan, p, &threads, &lists, &translations)
    })?;
    drop(lists);
    drop(lists_mem);

    let mut bases = Vec::with_capacity(merged.len());
    let mut total = 0;
    for partition in &merged {
        bases.push(total);
        total += partition.table.len();
    }
    let mut counts = Vec::with_capacity(plan.distinct_count);
    for slot in 0..plan.distinct_count {
        let group_counts: Vec<AtomicI64> = (0..total).map(|_| AtomicI64::new(0)).collect();
        parallel_tasks(ctx.threads, PARTITIONS, |q| {
            merge_distinct(q, slot, &threads, &translations, &bases, &group_counts)
        })?;
        counts.push(group_counts);
    }
    drop(threads);
    drop(translations);

    let finished = parallel_tasks(ctx.threads, merged.len(), |p| {
        finish_partition(&plan, &merged[p], &counts, bases[p])
    })?;
    drop(merged);
    let mut mem = new_reservation()?;
    let mut chunks = Vec::new();
    for chunk in finished.into_iter().flatten() {
        mem.grow(chunk_bytes(&chunk))?;
        if chunk.len <= CHUNK_ROWS {
            chunks.push(chunk);
        } else {
            let mut offset = 0;
            while offset < chunk.len {
                let len = CHUNK_ROWS.min(chunk.len - offset);
                chunks.push(chunk.slice(offset, len));
                offset += len;
            }
        }
    }
    Ok(Materialized {
        chunks,
        hold: vec![mem],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ExecStats;
    use crate::database::Database;
    use crate::pipeline::Source;
    use std::collections::{BTreeMap, BTreeSet};
    use veta_core::Value;
    use veta_core::rand::Rng;

    fn agg(func: AggFunc, distinct: bool, arg: Option<BoundExpr>) -> AggregateExpr {
        AggregateExpr::new(func, distinct, arg).unwrap()
    }

    fn run_on(
        chunks: Vec<Chunk>,
        groups: &[BoundExpr],
        aggregates: &[AggregateExpr],
        threads: usize,
    ) -> Vec<Vec<Value>> {
        let db = Database::open("unused");
        let stats = ExecStats::default();
        let ctx = ExecContext {
            db: &db,
            threads,
            stats: &stats,
            prune: true,
            profile: None,
        };
        let pipeline = Pipeline::new(Source::chunks(chunks));
        let out = run(pipeline, groups, aggregates, &ctx).unwrap();
        let mut rows: Vec<Vec<Value>> = out
            .chunks
            .iter()
            .flat_map(|c| (0..c.len).map(move |i| c.row(i)))
            .collect();
        rows.sort();
        rows
    }

    /// `k` Int64 keys with repeats, `s` strings with NULLs, `v` Int32 with NULLs.
    fn random_chunks(seed: u64, keys: i64) -> Vec<Chunk> {
        let mut rng = Rng::new(seed);
        let mut chunks = Vec::new();
        for _ in 0..40 {
            let rows: Vec<Vec<Value>> = (0..500)
                .map(|_| {
                    let k = rng.range_i64(0, keys - 1);
                    let s = if rng.next_bool(0.1) {
                        Value::Null
                    } else {
                        Value::varchar(format!("s{}", rng.below(50)))
                    };
                    let v = if rng.next_bool(0.2) {
                        Value::Null
                    } else {
                        Value::Int32(rng.range_i64(-100, 100) as i32)
                    };
                    vec![Value::Int64(k), s, v]
                })
                .collect();
            chunks.push(
                Chunk::from_rows(
                    &[DataType::Int64, DataType::Varchar, DataType::Int32],
                    &rows,
                )
                .unwrap(),
            );
        }
        chunks
    }

    fn rows_of(chunks: &[Chunk]) -> Vec<Vec<Value>> {
        chunks
            .iter()
            .flat_map(|c| (0..c.len).map(move |i| c.row(i)))
            .collect()
    }

    fn col(index: usize, data_type: DataType, nullable: bool) -> BoundExpr {
        BoundExpr::column(index, "c", data_type, nullable)
    }

    #[test]
    fn group_by_fixed_key_matches_a_hand_computation_for_any_thread_count() {
        let chunks = random_chunks(1, 5000);
        let mut expected: BTreeMap<i64, (i64, i64, Option<i64>, Option<i32>)> = BTreeMap::new();
        for row in rows_of(&chunks) {
            let Value::Int64(k) = row[0] else {
                panic!("key is not Int64");
            };
            let entry = expected.entry(k).or_insert((0, 0, None, None));
            entry.0 += 1;
            if let Value::Int32(v) = row[2] {
                entry.1 += 1;
                entry.2 = Some(entry.2.unwrap_or(0) + v as i64);
                entry.3 = Some(entry.3.map_or(v, |m: i32| m.max(v)));
            }
        }
        let want: Vec<Vec<Value>> = expected
            .iter()
            .map(|(&k, &(c, n, s, m))| {
                vec![
                    Value::Int64(k),
                    Value::Int64(c),
                    Value::Int64(n),
                    s.map_or(Value::Null, Value::Int64),
                    m.map_or(Value::Null, Value::Int32),
                ]
            })
            .collect();
        let aggregates = [
            agg(AggFunc::CountStar, false, None),
            agg(AggFunc::Count, false, Some(col(2, DataType::Int32, true))),
            agg(AggFunc::Sum, false, Some(col(2, DataType::Int32, true))),
            agg(AggFunc::Max, false, Some(col(2, DataType::Int32, true))),
        ];
        for threads in [1, 8] {
            let got = run_on(
                chunks.clone(),
                &[col(0, DataType::Int64, false)],
                &aggregates,
                threads,
            );
            assert_eq!(got, want);
        }
    }

    #[test]
    fn string_group_with_nulls_and_distinct_counts() {
        let chunks = random_chunks(2, 300);
        let mut expected: BTreeMap<Value, BTreeSet<i64>> = BTreeMap::new();
        for row in rows_of(&chunks) {
            let Value::Int64(k) = row[0] else {
                panic!("key is not Int64");
            };
            expected.entry(row[1].clone()).or_default().insert(k);
        }
        let want: Vec<Vec<Value>> = expected
            .iter()
            .map(|(s, set)| vec![s.clone(), Value::Int64(set.len() as i64)])
            .collect();
        let aggregates = [agg(
            AggFunc::Count,
            true,
            Some(col(0, DataType::Int64, false)),
        )];
        for threads in [1, 8] {
            let mut got = run_on(
                chunks.clone(),
                &[col(1, DataType::Varchar, true)],
                &aggregates,
                threads,
            );
            got.sort();
            let mut w = want.clone();
            w.sort();
            assert_eq!(got, w);
        }
    }

    #[test]
    fn global_aggregates_without_rows_give_one_row() {
        let aggregates = [
            agg(AggFunc::CountStar, false, None),
            agg(AggFunc::Sum, false, Some(col(0, DataType::Int64, true))),
            agg(AggFunc::Min, false, Some(col(0, DataType::Int64, true))),
            agg(AggFunc::Avg, false, Some(col(0, DataType::Int64, true))),
            agg(AggFunc::Count, true, Some(col(0, DataType::Int64, true))),
        ];
        let got = run_on(Vec::new(), &[], &aggregates, 4);
        assert_eq!(
            got,
            vec![vec![
                Value::Int64(0),
                Value::Null,
                Value::Null,
                Value::Null,
                Value::Int64(0),
            ]]
        );
    }

    #[test]
    fn global_distinct_of_strings_across_threads() {
        let chunks = random_chunks(3, 10);
        let distinct: BTreeSet<Value> = rows_of(&chunks)
            .into_iter()
            .map(|r| r[1].clone())
            .filter(|v| !v.is_null())
            .collect();
        let aggregates = [agg(
            AggFunc::Count,
            true,
            Some(col(1, DataType::Varchar, true)),
        )];
        for threads in [1, 8] {
            let got = run_on(chunks.clone(), &[], &aggregates, threads);
            assert_eq!(got, vec![vec![Value::Int64(distinct.len() as i64)]]);
        }
    }

    #[test]
    fn composite_group_of_string_and_integer() {
        let chunks = random_chunks(4, 20);
        let mut expected: BTreeMap<(Value, Value), i64> = BTreeMap::new();
        for row in rows_of(&chunks) {
            *expected
                .entry((row[1].clone(), row[0].clone()))
                .or_insert(0) += 1;
        }
        let mut want: Vec<Vec<Value>> = expected
            .iter()
            .map(|((s, k), &c)| vec![s.clone(), k.clone(), Value::Int64(c)])
            .collect();
        want.sort();
        let aggregates = [agg(AggFunc::CountStar, false, None)];
        for threads in [1, 8] {
            let got = run_on(
                chunks.clone(),
                &[
                    col(1, DataType::Varchar, true),
                    col(0, DataType::Int64, false),
                ],
                &aggregates,
                threads,
            );
            assert_eq!(got, want);
        }
    }

    #[test]
    fn decimal_sums_and_averages_are_exact() {
        let data_type = DataType::Decimal {
            precision: 10,
            scale: 2,
        };
        let chunk = Chunk::new(vec![Vector::from_data(VectorData::Decimal64 {
            values: vec![150, 250, -100],
            precision: 10,
            scale: 2,
        })])
        .unwrap();
        let aggregates = [
            agg(AggFunc::Sum, false, Some(col(0, data_type, false))),
            agg(AggFunc::Avg, false, Some(col(0, data_type, false))),
            agg(AggFunc::Max, false, Some(col(0, data_type, false))),
        ];
        let got = run_on(vec![chunk], &[], &aggregates, 1);
        assert_eq!(
            got,
            vec![vec![
                Value::Decimal {
                    value: 300,
                    precision: 38,
                    scale: 2
                },
                Value::Float64(1.0),
                Value::Decimal {
                    value: 250,
                    precision: 10,
                    scale: 2
                },
            ]]
        );
    }

    #[test]
    fn sum_of_distinct_is_unsupported() {
        let aggregates = [agg(
            AggFunc::Sum,
            true,
            Some(col(0, DataType::Int64, false)),
        )];
        let err = AggPlan::new(&[], &aggregates).unwrap_err();
        assert_eq!(err.to_string(), "unsupported: SUM(DISTINCT)");
    }

    #[test]
    fn float_extremes_follow_the_total_order() {
        let chunk = Chunk::new(vec![Vector::from_data(VectorData::Float64(vec![
            1.0,
            f64::NAN,
            -1.0,
        ]))])
        .unwrap();
        let aggregates = [
            agg(AggFunc::Min, false, Some(col(0, DataType::Float64, false))),
            agg(AggFunc::Max, false, Some(col(0, DataType::Float64, false))),
        ];
        let got = run_on(vec![chunk], &[], &aggregates, 1);
        assert_eq!(got[0][0], Value::Float64(-1.0));
        assert_eq!(got[0][1], Value::Float64(f64::NAN));
    }
}
