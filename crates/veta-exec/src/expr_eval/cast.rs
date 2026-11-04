//! `CAST` over vectors between every type of §4, with the same rules as the constant
//! folding of `veta_sql::expr::cast_value`: narrowing numeric conversions round half away
//! from zero and fail when the value does not fit; strings are trimmed and parsed
//! (`YYYY-MM-DD`, `YYYY-MM-DD HH:MM:SS`, numbers, `true/false/1/0`); a value that cannot be
//! converted is an error (exit 1). NULL rows are never converted.

use std::io::Write;

use veta_core::{Bitmap, DataType, Result, Vector, VectorData, VetaError, datetime, decimal};

use super::decimal_data;
use crate::functions::StringBuilder;

fn cannot(from: DataType, to: DataType) -> VetaError {
    VetaError::user(format!(
        "cannot cast {} to {}",
        from.sql_name(),
        to.sql_name()
    ))
}

/// `f` on every non-NULL value; NULL slots get `T::default()`.
fn try_map<U: Copy, T: Default>(
    src: &[U],
    validity: Option<&Bitmap>,
    mut f: impl FnMut(U) -> Result<T>,
) -> Result<Vec<T>> {
    let mut out = Vec::with_capacity(src.len());
    match validity {
        None => {
            for &x in src {
                out.push(f(x)?);
            }
        }
        Some(b) => {
            for (i, &x) in src.iter().enumerate() {
                out.push(if b.get(i) { f(x)? } else { T::default() });
            }
        }
    }
    Ok(out)
}

/// `f` on every non-NULL string.
fn try_map_str<T: Default>(
    offsets: &[u32],
    bytes: &[u8],
    validity: Option<&Bitmap>,
    mut f: impl FnMut(&str) -> Result<T>,
) -> Result<Vec<T>> {
    let n = offsets.len().saturating_sub(1);
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        if validity.is_some_and(|b| !b.get(i)) {
            out.push(T::default());
            continue;
        }
        let text = String::from_utf8_lossy(&bytes[offsets[i] as usize..offsets[i + 1] as usize]);
        out.push(f(text.trim())?);
    }
    Ok(out)
}

fn range(to: DataType) -> (i128, i128) {
    match to {
        DataType::Int16 => (i16::MIN as i128, i16::MAX as i128),
        DataType::Int32 => (i32::MIN as i128, i32::MAX as i128),
        DataType::Int64 => (i64::MIN as i128, i64::MAX as i128),
        _ => (i128::MIN, i128::MAX),
    }
}

fn out_of_range(v: i128, to: DataType) -> VetaError {
    VetaError::user(format!("value {v} out of range for {}", to.sql_name()))
}

/// Integer vector of type `to` from `i128` values, checking the range of non-NULL rows.
fn ints(values: Vec<i128>, to: DataType, validity: Option<&Bitmap>) -> Result<VectorData> {
    let (lo, hi) = range(to);
    let check = |i: usize, v: i128| -> Result<()> {
        if (v < lo || v > hi) && validity.is_none_or(|b| b.get(i)) {
            return Err(out_of_range(v, to));
        }
        Ok(())
    };
    for (i, &v) in values.iter().enumerate() {
        check(i, v)?;
    }
    Ok(match to {
        DataType::Int16 => VectorData::Int16(values.iter().map(|&v| v as i16).collect()),
        DataType::Int32 => VectorData::Int32(values.iter().map(|&v| v as i32).collect()),
        DataType::Int64 => VectorData::Int64(values.iter().map(|&v| v as i64).collect()),
        DataType::Int128 => VectorData::Int128(values),
        _ => return Err(VetaError::internal(format!("{to} is not an integer type"))),
    })
}

fn decimals(
    values: Vec<i128>,
    precision: u8,
    scale: u8,
    validity: Option<&Bitmap>,
) -> Result<VectorData> {
    for (i, &v) in values.iter().enumerate() {
        if !decimal::fits_precision(v, precision) && validity.is_none_or(|b| b.get(i)) {
            return Err(VetaError::user("decimal overflow"));
        }
    }
    Ok(decimal_data(values, precision, scale))
}

fn f64_to_i128(f: f64) -> Result<i128> {
    let r = f.round();
    if !r.is_finite() || r.abs() >= 1.7e38 {
        return Err(VetaError::user(format!("value {f} out of integer range")));
    }
    Ok(r as i128)
}

/// Integer values of an integer or boolean vector, widened to `i128`.
fn int_values(data: &VectorData) -> Option<Vec<i128>> {
    Some(match data {
        VectorData::Boolean(x) => x.iter().map(|&b| b as i128).collect(),
        VectorData::Int16(x) => x.iter().map(|&v| v as i128).collect(),
        VectorData::Int32(x) => x.iter().map(|&v| v as i128).collect(),
        VectorData::Int64(x) => x.iter().map(|&v| v as i128).collect(),
        VectorData::Int128(x) => x.clone(),
        _ => return None,
    })
}

/// Unscaled values and scale of a decimal vector.
fn decimal_values(data: &VectorData) -> Option<(Vec<i128>, u8)> {
    match data {
        VectorData::Decimal64 { values, scale, .. } => {
            Some((values.iter().map(|&v| v as i128).collect(), *scale))
        }
        VectorData::Decimal128 { values, scale, .. } => Some((values.clone(), *scale)),
        _ => None,
    }
}

/// Text of every row (`Varchar` output), in the format `cast_value` uses.
fn to_text(v: &Vector) -> Result<VectorData> {
    let n = v.len();
    let mut out = StringBuilder::with_capacity(n, n * 8);
    macro_rules! each {
        ($x:expr, |$e:ident, $buf:ident| $write:expr) => {{
            for (i, &$e) in $x.iter().enumerate() {
                if v.is_valid(i) {
                    let $buf = out.bytes_mut();
                    $write;
                }
                out.finish_row()?;
            }
        }};
    }
    match &v.data {
        VectorData::Boolean(x) => each!(x, |b, buf| buf.extend_from_slice(if b {
            &b"true"[..]
        } else {
            &b"false"[..]
        })),
        VectorData::Int16(x) => each!(x, |a, buf| write!(buf, "{a}")?),
        VectorData::Int32(x) => each!(x, |a, buf| write!(buf, "{a}")?),
        VectorData::Int64(x) => each!(x, |a, buf| write!(buf, "{a}")?),
        VectorData::Int128(x) => each!(x, |a, buf| write!(buf, "{a}")?),
        VectorData::Float64(x) => each!(x, |a, buf| write!(buf, "{a}")?),
        VectorData::Decimal64 { values, scale, .. } => {
            each!(values, |a, buf| decimal::write(buf, a as i128, *scale))
        }
        VectorData::Decimal128 { values, scale, .. } => {
            each!(values, |a, buf| decimal::write(buf, a, *scale))
        }
        VectorData::Date(x) => each!(x, |d, buf| datetime::write_date(buf, d)),
        VectorData::Timestamp(x) => each!(x, |t, buf| datetime::write_timestamp(buf, t)),
        VectorData::Varchar { .. } => return Ok(v.data.clone()),
    }
    Ok(out.finish())
}

/// Parses every non-NULL string as `to`.
fn from_text(
    offsets: &[u32],
    bytes: &[u8],
    validity: Option<&Bitmap>,
    to: DataType,
) -> Result<VectorData> {
    let invalid = |what: &str, text: &str| VetaError::user(format!("invalid {what}: {text}"));
    Ok(match to {
        DataType::Boolean => {
            VectorData::Boolean(try_map_str(offsets, bytes, validity, |t| {
                match t.to_ascii_lowercase().as_str() {
                    "true" | "1" => Ok(true),
                    "false" | "0" => Ok(false),
                    _ => Err(invalid("boolean", t)),
                }
            })?)
        }
        DataType::Int16 | DataType::Int32 | DataType::Int64 | DataType::Int128 => {
            let values = try_map_str(offsets, bytes, validity, |t| {
                t.parse::<i128>().map_err(|_| invalid("integer", t))
            })?;
            ints(values, to, validity)?
        }
        DataType::Float64 => VectorData::Float64(try_map_str(offsets, bytes, validity, |t| {
            t.parse::<f64>().map_err(|_| invalid("number", t))
        })?),
        DataType::Decimal { precision, scale } => {
            let values = try_map_str(offsets, bytes, validity, |t| {
                decimal::parse(t, precision, scale)
            })?;
            decimal_data(values, precision, scale)
        }
        DataType::Date => VectorData::Date(try_map_str(offsets, bytes, validity, |t| {
            datetime::parse_date(t.as_bytes())
        })?),
        DataType::Timestamp => VectorData::Timestamp(try_map_str(offsets, bytes, validity, |t| {
            datetime::parse_timestamp(t.as_bytes())
        })?),
        DataType::Varchar => VectorData::Varchar {
            offsets: offsets.to_vec(),
            bytes: bytes.to_vec(),
        },
    })
}

/// Converts every row of `v` to `to`; NULL stays NULL.
pub fn cast_vector(v: &Vector, to: DataType) -> Result<Vector> {
    let from = v.data_type();
    if from == to {
        return Ok(v.clone());
    }
    let validity = v.validity.as_ref();
    let data = match (&v.data, to) {
        // Widening integer conversions, the ones the binder inserts most.
        (VectorData::Int16(x), DataType::Int32) => {
            VectorData::Int32(x.iter().map(|&a| a as i32).collect())
        }
        (VectorData::Int16(x), DataType::Int64) => {
            VectorData::Int64(x.iter().map(|&a| a as i64).collect())
        }
        (VectorData::Int32(x), DataType::Int64) => {
            VectorData::Int64(x.iter().map(|&a| a as i64).collect())
        }
        (VectorData::Int64(x), DataType::Int128) => {
            VectorData::Int128(x.iter().map(|&a| a as i128).collect())
        }
        (VectorData::Int16(x), DataType::Float64) => {
            VectorData::Float64(x.iter().map(|&a| a as f64).collect())
        }
        (VectorData::Int32(x), DataType::Float64) => {
            VectorData::Float64(x.iter().map(|&a| a as f64).collect())
        }
        (VectorData::Int64(x), DataType::Float64) => {
            VectorData::Float64(x.iter().map(|&a| a as f64).collect())
        }
        (VectorData::Int128(x), DataType::Float64) => {
            VectorData::Float64(x.iter().map(|&a| a as f64).collect())
        }
        (VectorData::Date(x), DataType::Timestamp) => VectorData::Timestamp(
            x.iter()
                .map(|&d| d as i64 * datetime::SECONDS_PER_DAY)
                .collect(),
        ),
        (VectorData::Timestamp(x), DataType::Date) => VectorData::Date(
            x.iter()
                .map(|&t| t.div_euclid(datetime::SECONDS_PER_DAY) as i32)
                .collect(),
        ),
        (_, DataType::Varchar) => to_text(v)?,
        (VectorData::Varchar { offsets, bytes }, _) => from_text(offsets, bytes, validity, to)?,
        (VectorData::Float64(x), DataType::Decimal { precision, scale }) => {
            let factor = decimal::pow10(scale) as f64;
            let values = try_map(x, validity, |f| f64_to_i128(f * factor))?;
            decimals(values, precision, scale, validity)?
        }
        (VectorData::Float64(x), t) if t.is_integer() => {
            let values = try_map(x, validity, f64_to_i128)?;
            ints(values, to, validity)?
        }
        (VectorData::Decimal64 { .. } | VectorData::Decimal128 { .. }, _) => {
            let (values, scale) = decimal_values(&v.data).ok_or_else(|| cannot(from, to))?;
            match to {
                DataType::Float64 => {
                    VectorData::Float64(values.iter().map(|&a| decimal::to_f64(a, scale)).collect())
                }
                DataType::Decimal {
                    precision,
                    scale: s,
                } => {
                    let values = try_map(&values, validity, |a| decimal::rescale(a, scale, s))?;
                    decimals(values, precision, s, validity)?
                }
                t if t.is_integer() => {
                    let values = try_map(&values, validity, |a| decimal::rescale(a, scale, 0))?;
                    ints(values, to, validity)?
                }
                _ => return Err(cannot(from, to)),
            }
        }
        (VectorData::Boolean(_), t) if t.is_integer() => {
            let values = int_values(&v.data).ok_or_else(|| cannot(from, to))?;
            ints(values, to, validity)?
        }
        (_, DataType::Boolean) if from.is_integer() => {
            let values = int_values(&v.data).ok_or_else(|| cannot(from, to))?;
            VectorData::Boolean(values.iter().map(|&a| a != 0).collect())
        }
        (_, DataType::Decimal { precision, scale }) if from.is_integer() => {
            let values = int_values(&v.data).ok_or_else(|| cannot(from, to))?;
            let factor = decimal::pow10(scale);
            let values = try_map(&values, validity, |a| {
                a.checked_mul(factor)
                    .ok_or_else(|| VetaError::user("decimal overflow"))
            })?;
            decimals(values, precision, scale, validity)?
        }
        (_, t) if from.is_integer() && t.is_integer() => {
            let values = int_values(&v.data).ok_or_else(|| cannot(from, to))?;
            ints(values, to, validity)?
        }
        _ => return Err(cannot(from, to)),
    };
    Vector::new(data, v.validity.clone())
}
