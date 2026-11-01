//! Exact decimals: literal parsing, formatting, rescaling and checked arithmetic (contract §4).
//!
//! A decimal is an `i128` unscaled value with a precision and a scale. Result types follow
//! §4; the precision is capped at 38 and a value that does not fit its precision is an
//! overflow error (exit 1), like integer overflow.

use crate::error::{Result, VetaError};
use crate::types::MAX_DECIMAL_PRECISION;

const POW10: [i128; 39] = {
    let mut table = [1i128; 39];
    let mut i = 1;
    while i < 39 {
        table[i] = table[i - 1] * 10;
        i += 1;
    }
    table
};

/// `10^n` for `n ≤ 38`.
pub fn pow10(n: u8) -> i128 {
    POW10[n as usize]
}

fn overflow() -> VetaError {
    VetaError::user("decimal overflow")
}

/// Whether `value` has at most `precision` digits.
pub fn fits_precision(value: i128, precision: u8) -> bool {
    value.unsigned_abs() < pow10(precision.min(MAX_DECIMAL_PRECISION)) as u128
}

fn check_precision(value: i128, precision: u8) -> Result<i128> {
    if fits_precision(value, precision) {
        Ok(value)
    } else {
        Err(overflow())
    }
}

/// A parsed decimal literal: unscaled value, precision and scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecimalLiteral {
    pub value: i128,
    pub precision: u8,
    pub scale: u8,
}

/// Splits `[+-]digits[.digits]` into sign, integer digits and fraction digits.
fn split_number(text: &str) -> Option<(bool, &str, &str)> {
    let (negative, rest) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let (int, frac) = match rest.find('.') {
        Some(dot) => (&rest[..dot], &rest[dot + 1..]),
        None => (rest, ""),
    };
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if (int.is_empty() && frac.is_empty()) || !digits(int) || !digits(frac) {
        return None;
    }
    Some((negative, int, frac))
}

fn accumulate(acc: i128, digits: &str) -> Option<i128> {
    digits.bytes().try_fold(acc, |acc, b| {
        acc.checked_mul(10)?.checked_add((b - b'0') as i128)
    })
}

/// Parses a SQL numeric literal such as `0.05` into its natural decimal type: scale is the
/// number of fraction digits and precision the integer digits (leading zeros dropped, at
/// least one) plus the scale, so `0.05` is `DECIMAL(3,2)` (contract §4).
pub fn parse_literal(text: &str) -> Result<DecimalLiteral> {
    let invalid = || VetaError::user(format!("invalid decimal literal: {text}"));
    let (negative, int, frac) = split_number(text).ok_or_else(invalid)?;
    let int_digits = int.trim_start_matches('0').len().max(1);
    let precision = int_digits + frac.len();
    if precision > MAX_DECIMAL_PRECISION as usize {
        return Err(VetaError::user(format!(
            "decimal literal exceeds precision 38: {text}"
        )));
    }
    let magnitude =
        accumulate(accumulate(0, int).ok_or_else(overflow)?, frac).ok_or_else(overflow)?;
    Ok(DecimalLiteral {
        value: if negative { -magnitude } else { magnitude },
        precision: precision as u8,
        scale: frac.len() as u8,
    })
}

/// Parses text into `DECIMAL(precision, scale)`; extra fraction digits round half away
/// from zero and a value with too many digits is an overflow.
pub fn parse(text: &str, precision: u8, scale: u8) -> Result<i128> {
    let invalid = || VetaError::user(format!("invalid decimal: {text}"));
    let (negative, int, frac) = split_number(text.trim()).ok_or_else(invalid)?;
    let kept = &frac[..frac.len().min(scale as usize)];
    let mut magnitude =
        accumulate(accumulate(0, int).ok_or_else(overflow)?, kept).ok_or_else(overflow)?;
    magnitude = magnitude
        .checked_mul(pow10(scale - kept.len() as u8))
        .ok_or_else(overflow)?;
    if frac.len() > scale as usize && frac.as_bytes()[scale as usize] >= b'5' {
        magnitude = magnitude.checked_add(1).ok_or_else(overflow)?;
    }
    check_precision(if negative { -magnitude } else { magnitude }, precision)
}

/// Text with exactly `scale` fraction digits (contract §13.1).
pub fn format(value: i128, scale: u8) -> String {
    let mut out = Vec::new();
    write(&mut out, value, scale);
    String::from_utf8(out).unwrap_or_default()
}

/// Appends the text of [`format`] to `out`.
pub fn write(out: &mut Vec<u8>, value: i128, scale: u8) {
    if value < 0 {
        out.push(b'-');
    }
    let digits = value.unsigned_abs().to_string();
    let scale = scale as usize;
    if scale == 0 {
        out.extend_from_slice(digits.as_bytes());
        return;
    }
    if digits.len() <= scale {
        out.extend_from_slice(b"0.");
        out.extend(std::iter::repeat_n(b'0', scale - digits.len()));
        out.extend_from_slice(digits.as_bytes());
    } else {
        let (int, frac) = digits.split_at(digits.len() - scale);
        out.extend_from_slice(int.as_bytes());
        out.push(b'.');
        out.extend_from_slice(frac.as_bytes());
    }
}

/// Changes the scale. Increasing it is exact (overflow is an error); decreasing it rounds
/// half away from zero.
pub fn rescale(value: i128, from_scale: u8, to_scale: u8) -> Result<i128> {
    if to_scale >= from_scale {
        value
            .checked_mul(pow10(to_scale - from_scale))
            .ok_or_else(overflow)
    } else {
        let divisor = pow10(from_scale - to_scale);
        let quotient = value / divisor;
        let remainder = (value % divisor).unsigned_abs();
        if remainder * 2 >= divisor as u128 {
            Ok(quotient + value.signum())
        } else {
            Ok(quotient)
        }
    }
}

/// Approximate `f64` value of `value / 10^scale`.
pub fn to_f64(value: i128, scale: u8) -> f64 {
    value as f64 / pow10(scale) as f64
}

fn cap(precision: u32, scale: u32) -> Result<(u8, u8)> {
    if scale > MAX_DECIMAL_PRECISION as u32 {
        return Err(VetaError::user(format!(
            "decimal scale {scale} exceeds precision 38"
        )));
    }
    Ok((
        precision.min(MAX_DECIMAL_PRECISION as u32) as u8,
        scale as u8,
    ))
}

/// Result type of `+`/`-`: scale `max(s1,s2)`, precision `max(p1−s1, p2−s2) + max(s1,s2) + 1`.
pub fn add_type(p1: u8, s1: u8, p2: u8, s2: u8) -> Result<(u8, u8)> {
    let scale = s1.max(s2) as u32;
    let int = (p1 - s1).max(p2 - s2) as u32;
    cap(int + scale + 1, scale)
}

/// Result type of `*`: scale `s1+s2`, precision `p1+p2`.
pub fn mul_type(p1: u8, s1: u8, p2: u8, s2: u8) -> Result<(u8, u8)> {
    cap(p1 as u32 + p2 as u32, s1 as u32 + s2 as u32)
}

/// `a + b` at scale `max(sa, sb)`, checked against `precision`.
pub fn add(a: i128, sa: u8, b: i128, sb: u8, precision: u8) -> Result<i128> {
    let scale = sa.max(sb);
    let sum = rescale(a, sa, scale)?
        .checked_add(rescale(b, sb, scale)?)
        .ok_or_else(overflow)?;
    check_precision(sum, precision)
}

/// `a - b` at scale `max(sa, sb)`, checked against `precision`.
pub fn sub(a: i128, sa: u8, b: i128, sb: u8, precision: u8) -> Result<i128> {
    let scale = sa.max(sb);
    let diff = rescale(a, sa, scale)?
        .checked_sub(rescale(b, sb, scale)?)
        .ok_or_else(overflow)?;
    check_precision(diff, precision)
}

/// `a * b` at scale `sa + sb`, checked against `precision`.
pub fn mul(a: i128, b: i128, precision: u8) -> Result<i128> {
    check_precision(a.checked_mul(b).ok_or_else(overflow)?, precision)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_types() {
        assert_eq!(
            parse_literal("0.05").unwrap(),
            DecimalLiteral {
                value: 5,
                precision: 3,
                scale: 2
            }
        );
        assert_eq!(
            parse_literal("123.450").unwrap(),
            DecimalLiteral {
                value: 123450,
                precision: 6,
                scale: 3
            }
        );
        assert_eq!(parse_literal("-1.5").unwrap().value, -15);
        assert_eq!(parse_literal("7").unwrap().scale, 0);
        assert_eq!(parse_literal(".5").unwrap().precision, 2);
        assert!(parse_literal("1.2.3").is_err());
        assert!(parse_literal("").is_err());
        assert!(parse_literal(".").is_err());
        assert!(parse_literal("1e5").is_err());
        assert!(parse_literal("1234567890123456789012345678901234567.89").is_err());
    }

    #[test]
    fn parse_with_target_type() {
        assert_eq!(parse("12.34", 15, 2).unwrap(), 1234);
        assert_eq!(parse("12.3", 15, 2).unwrap(), 1230);
        assert_eq!(parse("-0.5", 3, 2).unwrap(), -50);
        assert_eq!(parse("1.235", 4, 2).unwrap(), 124);
        assert_eq!(parse("-1.235", 4, 2).unwrap(), -124);
        assert_eq!(parse("1.234", 4, 2).unwrap(), 123);
        assert_eq!(parse("42", 5, 0).unwrap(), 42);
        assert!(parse("1000", 5, 2).is_err());
        assert!(parse("abc", 5, 2).is_err());
    }

    #[test]
    fn formats_exactly_scale_digits() {
        assert_eq!(format(5, 2), "0.05");
        assert_eq!(format(-5, 2), "-0.05");
        assert_eq!(format(0, 2), "0.00");
        assert_eq!(format(-1234, 2), "-12.34");
        assert_eq!(format(1230, 2), "12.30");
        assert_eq!(format(12345, 0), "12345");
        assert_eq!(format(-7, 0), "-7");
        assert_eq!(format(1, 38), format!("0.{}1", "0".repeat(37)));
        assert_eq!(
            format(i128::MIN, 0),
            "-170141183460469231731687303715884105728"
        );
    }

    #[test]
    fn rescale_up_down_and_overflow() {
        assert_eq!(rescale(5, 2, 4).unwrap(), 500);
        assert_eq!(rescale(500, 4, 2).unwrap(), 5);
        assert_eq!(rescale(1235, 3, 2).unwrap(), 124);
        assert_eq!(rescale(-1235, 3, 2).unwrap(), -124);
        assert_eq!(rescale(1234, 3, 2).unwrap(), 123);
        assert_eq!(rescale(-1234, 3, 0).unwrap(), -1);
        assert_eq!(rescale(7, 1, 1).unwrap(), 7);
        let err = rescale(i128::MAX / 10, 0, 2).unwrap_err();
        assert_eq!(err.to_string(), "decimal overflow");
        assert_eq!(err.exit_code(), 1);
        assert!(rescale(1, 0, 38).is_ok());
    }

    #[test]
    fn result_types() {
        assert_eq!(add_type(15, 2, 3, 2).unwrap(), (16, 2));
        assert_eq!(add_type(10, 0, 5, 4).unwrap(), (15, 4));
        assert_eq!(mul_type(15, 2, 16, 2).unwrap(), (31, 4));
        // Precision caps at 38.
        assert_eq!(mul_type(31, 4, 16, 2).unwrap(), (38, 6));
        assert_eq!(add_type(38, 2, 38, 2).unwrap(), (38, 2));
        assert!(mul_type(38, 20, 38, 20).is_err());
    }

    #[test]
    fn checked_arithmetic() {
        assert_eq!(add(105, 2, 5, 1, 4).unwrap(), 155);
        assert_eq!(sub(105, 2, 5, 1, 4).unwrap(), 55);
        assert_eq!(mul(150, 25, 6).unwrap(), 3750);
        let max38 = pow10(38) - 1;
        assert!(add(max38, 0, 1, 0, 38).is_err());
        assert!(sub(-max38, 0, 1, 0, 38).is_err());
        assert!(add(999, 0, 1, 0, 3).is_err());
        assert!(mul(pow10(20), pow10(19), 38).is_err());
        assert!(mul(i128::MAX, 2, 38).is_err());
    }

    #[test]
    fn precision_check() {
        assert!(fits_precision(999, 3));
        assert!(!fits_precision(1000, 3));
        assert!(fits_precision(-999, 3));
        assert!(!fits_precision(-1000, 3));
    }
}
