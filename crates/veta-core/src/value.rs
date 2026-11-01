//! Scalar values with a total order for sorting and grouping (contract §4).
//!
//! The order and equality here are the sort/grouping ones, not SQL `=`: `NULL` equals
//! `NULL` and sorts after every other value, so ascending order puts NULL last and
//! descending order (the reverse) puts it first, as §4 requires. Three-valued logic lives
//! in the executor.

use std::cmp::Ordering;

use crate::decimal;
use crate::types::{DataType, MAX_DECIMAL64_PRECISION};

#[derive(Debug, Clone)]
pub enum Value {
    Null,
    Boolean(bool),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    Int128(i128),
    Float64(f64),
    Decimal {
        value: i128,
        precision: u8,
        scale: u8,
    },
    Varchar(Vec<u8>),
    /// Days since 1970-01-01.
    Date(i32),
    /// Seconds since 1970-01-01 00:00:00 UTC.
    Timestamp(i64),
}

impl Value {
    pub fn varchar(s: impl AsRef<[u8]>) -> Value {
        Value::Varchar(s.as_ref().to_vec())
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Type of the value; `None` for `NULL`, which has no type of its own.
    pub fn data_type(&self) -> Option<DataType> {
        Some(match self {
            Value::Null => return None,
            Value::Boolean(_) => DataType::Boolean,
            Value::Int16(_) => DataType::Int16,
            Value::Int32(_) => DataType::Int32,
            Value::Int64(_) => DataType::Int64,
            Value::Int128(_) => DataType::Int128,
            Value::Float64(_) => DataType::Float64,
            Value::Decimal {
                precision, scale, ..
            } => DataType::Decimal {
                precision: *precision,
                scale: *scale,
            },
            Value::Varchar(_) => DataType::Varchar,
            Value::Date(_) => DataType::Date,
            Value::Timestamp(_) => DataType::Timestamp,
        })
    }

    /// The integer value of `Int16`..`Int128`.
    pub fn as_i128(&self) -> Option<i128> {
        match self {
            Value::Int16(v) => Some(*v as i128),
            Value::Int32(v) => Some(*v as i128),
            Value::Int64(v) => Some(*v as i128),
            Value::Int128(v) => Some(*v),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Varchar(b) => Some(b),
            _ => None,
        }
    }

    /// Whether a decimal value uses the `i64` physical representation.
    pub fn is_narrow_decimal(precision: u8) -> bool {
        precision <= MAX_DECIMAL64_PRECISION
    }

    fn class(&self) -> Class {
        match self {
            Value::Boolean(_) => Class::Boolean,
            Value::Int16(_)
            | Value::Int32(_)
            | Value::Int64(_)
            | Value::Int128(_)
            | Value::Float64(_)
            | Value::Decimal { .. } => Class::Numeric,
            Value::Varchar(_) => Class::Varchar,
            Value::Date(_) | Value::Timestamp(_) => Class::Temporal,
            Value::Null => Class::Null,
        }
    }
}

/// Rank of each family of values; values of different families never compare equal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Class {
    Boolean,
    Numeric,
    Varchar,
    Temporal,
    Null,
}

/// Exact numeric view used to compare across integer, decimal and float types.
enum Num {
    /// `value / 10^scale`.
    Scaled(i128, u8),
    Float(f64),
}

fn num(v: &Value) -> Num {
    match v {
        Value::Float64(f) => Num::Float(*f),
        Value::Decimal { value, scale, .. } => Num::Scaled(*value, *scale),
        other => Num::Scaled(other.as_i128().unwrap_or(0), 0),
    }
}

/// Total order on `f64`: `-0.0 = 0.0`, every NaN equal and greater than `+inf`.
pub fn cmp_f64(a: f64, b: f64) -> Ordering {
    match (a.is_nan(), b.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => a.partial_cmp(&b).unwrap_or(Ordering::Equal),
    }
}

/// Exact comparison of two scaled integers `a / 10^sa` and `b / 10^sb`.
pub fn cmp_scaled(a: i128, sa: u8, b: i128, sb: u8) -> Ordering {
    match sa.cmp(&sb) {
        Ordering::Equal => a.cmp(&b),
        Ordering::Less => match a.checked_mul(decimal::pow10(sb - sa)) {
            Some(a) => a.cmp(&b),
            // |a · 10^k| exceeds i128, so it exceeds |b| too.
            None => {
                if a > 0 {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
        },
        Ordering::Greater => cmp_scaled(b, sb, a, sa).reverse(),
    }
}

/// Exact comparison of an integer with a float (NaN is greater than every integer).
fn cmp_i128_f64(a: i128, b: f64) -> Ordering {
    if b.is_nan() {
        return Ordering::Less;
    }
    // 2^127 is exactly representable; every i128 is below it and at or above -2^127.
    const TWO_127: f64 = 170_141_183_460_469_231_731_687_303_715_884_105_728.0;
    if b >= TWO_127 {
        return Ordering::Less;
    }
    if b < -TWO_127 {
        return Ordering::Greater;
    }
    let t = b.trunc();
    let ti = t as i128;
    match a.cmp(&ti) {
        Ordering::Equal => {
            let frac = b - t;
            if frac > 0.0 {
                Ordering::Less
            } else if frac < 0.0 {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }
        other => other,
    }
}

fn cmp_num(a: &Value, b: &Value) -> Ordering {
    match (num(a), num(b)) {
        (Num::Scaled(x, sx), Num::Scaled(y, sy)) => cmp_scaled(x, sx, y, sy),
        (Num::Float(x), Num::Float(y)) => cmp_f64(x, y),
        (Num::Scaled(x, 0), Num::Float(y)) => cmp_i128_f64(x, y),
        (Num::Float(x), Num::Scaled(y, 0)) => cmp_i128_f64(y, x).reverse(),
        // Decimal against float compares through f64, as SQL engines do.
        (Num::Scaled(x, sx), Num::Float(y)) => cmp_f64(decimal::to_f64(x, sx), y),
        (Num::Float(x), Num::Scaled(y, sy)) => cmp_f64(x, decimal::to_f64(y, sy)),
    }
}

/// Timestamps in seconds; dates promote to midnight.
fn seconds(v: &Value) -> i64 {
    match v {
        Value::Date(d) => *d as i64 * 86_400,
        Value::Timestamp(t) => *t,
        _ => 0,
    }
}

impl Ord for Value {
    fn cmp(&self, other: &Self) -> Ordering {
        let (ca, cb) = (self.class(), other.class());
        if ca != cb {
            return ca.cmp(&cb);
        }
        match (self, other) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Boolean(a), Value::Boolean(b)) => a.cmp(b),
            (Value::Varchar(a), Value::Varchar(b)) => a.as_slice().cmp(b.as_slice()),
            (Value::Date(a), Value::Date(b)) => a.cmp(b),
            (Value::Timestamp(a), Value::Timestamp(b)) => a.cmp(b),
            _ if ca == Class::Temporal => seconds(self).cmp(&seconds(other)),
            _ => cmp_num(self, other),
        }
    }
}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Value {}

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

    #[test]
    fn null_sorts_last_ascending_and_first_descending() {
        let mut values = vec![
            Value::Int64(3),
            Value::Null,
            Value::Int64(-1),
            Value::Null,
            Value::Int64(2),
        ];
        values.sort();
        assert_eq!(
            values,
            vec![
                Value::Int64(-1),
                Value::Int64(2),
                Value::Int64(3),
                Value::Null,
                Value::Null
            ]
        );
        values.sort_by(|a, b| b.cmp(a));
        assert!(values[0].is_null() && values[1].is_null());
        assert_eq!(values[2], Value::Int64(3));
    }

    #[test]
    fn integers_compare_across_widths() {
        assert_eq!(Value::Int16(5), Value::Int64(5));
        assert_eq!(Value::Int32(-7), Value::Int128(-7));
        assert!(Value::Int16(-1) < Value::Int64(0));
        assert!(Value::Int128(i128::MAX) > Value::Int64(i64::MAX));
    }

    #[test]
    fn decimals_rescale_without_loss() {
        assert_eq!(dec(500, 5, 2), dec(5, 3, 0));
        assert_eq!(dec(5, 3, 2), dec(50, 4, 3));
        assert!(dec(5, 3, 2) < dec(6, 3, 2));
        assert!(dec(1, 2, 1) < Value::Int64(1));
        assert_eq!(dec(100, 3, 2), Value::Int32(1));
        // Rescaling i128::MAX would overflow; the comparison is still correct.
        assert!(dec(i128::MAX, 38, 0) > dec(1, 38, 10));
        assert!(dec(i128::MIN + 1, 38, 0) < dec(-1, 38, 10));
    }

    #[test]
    fn floats_order_with_nan_last() {
        assert_eq!(Value::Float64(0.0), Value::Float64(-0.0));
        assert_eq!(Value::Float64(f64::NAN), Value::Float64(f64::NAN));
        assert!(Value::Float64(f64::INFINITY) < Value::Float64(f64::NAN));
        assert!(Value::Float64(f64::NAN) < Value::Null);
        assert!(Value::Float64(f64::NEG_INFINITY) < Value::Float64(-1e300));
    }

    #[test]
    fn integers_compare_exactly_with_floats() {
        assert_eq!(Value::Int64(3), Value::Float64(3.0));
        assert!(Value::Int64(3) < Value::Float64(3.5));
        assert!(Value::Int64(-3) > Value::Float64(-3.5));
        assert!(Value::Int64(i64::MAX) < Value::Float64(9.3e18));
        // 2^53 + 1 is not representable: it is greater than the float 2^53.
        assert!(Value::Int64((1 << 53) + 1) > Value::Float64(9007199254740992.0));
        assert!(Value::Int128(i128::MAX) < Value::Float64(f64::INFINITY));
        assert!(Value::Int64(0) < Value::Float64(f64::NAN));
        assert!(dec(15, 2, 1) < Value::Float64(1.6));
    }

    #[test]
    fn strings_compare_bytewise() {
        assert!(Value::varchar("B") < Value::varchar("a"));
        assert!(Value::varchar("ab") < Value::varchar("abc"));
        assert!(Value::Varchar(vec![0x7f]) < Value::Varchar(vec![0x80]));
        assert_eq!(Value::varchar(""), Value::Varchar(Vec::new()));
    }

    #[test]
    fn dates_promote_to_timestamps() {
        assert_eq!(Value::Date(1), Value::Timestamp(86_400));
        assert!(Value::Date(1) < Value::Timestamp(86_401));
        assert!(Value::Date(-1) < Value::Timestamp(0));
    }

    #[test]
    fn families_do_not_mix() {
        assert!(Value::Boolean(true) < Value::Int64(0));
        assert!(Value::Int64(i64::MAX) < Value::varchar(""));
        assert!(Value::varchar("z") < Value::Date(0));
        assert_ne!(Value::Boolean(false), Value::Int64(0));
    }

    #[test]
    fn data_types() {
        assert_eq!(Value::Null.data_type(), None);
        assert_eq!(
            dec(1, 9, 2).data_type(),
            Some(DataType::Decimal {
                precision: 9,
                scale: 2
            })
        );
        assert_eq!(Value::Date(0).data_type(), Some(DataType::Date));
    }
}
