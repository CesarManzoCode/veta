//! TSV values and rows like ClickHouse `TSV` (contract §13.1): `\t` between columns, `\n`
//! after each row, NULL as `\N`, strings with `\\ \t \n \r \0 \b \f` escaped.
//!
//! Parsing (for `veta load --tsv`) is the inverse and also accepts `\'`.

use std::io::Write;

use crate::datetime;
use crate::decimal;
use crate::error::{Result, VetaError};
use crate::types::DataType;
use crate::value::Value;
use crate::vector::Chunk;

/// Appends a string with the TSV escapes.
pub fn write_escaped(out: &mut Vec<u8>, bytes: &[u8]) {
    for &b in bytes {
        match b {
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            0 => out.extend_from_slice(b"\\0"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0C => out.extend_from_slice(b"\\f"),
            _ => out.push(b),
        }
    }
}

/// Shortest text that reads back as the same `f64`; exponent form outside `[1e-5, 1e16)`.
pub fn write_f64(out: &mut Vec<u8>, f: f64) {
    let _ = if f.is_nan() {
        out.write_all(b"nan")
    } else if f.is_infinite() {
        out.write_all(if f > 0.0 { &b"inf"[..] } else { &b"-inf"[..] })
    } else {
        let a = f.abs();
        if a == 0.0 || (1e-5..1e16).contains(&a) {
            write!(out, "{f}")
        } else {
            write!(out, "{f:e}")
        }
    };
}

/// Appends one value in the format of §13.1.
pub fn write_value(out: &mut Vec<u8>, value: &Value) {
    let _ = match value {
        Value::Null => out.write_all(b"\\N"),
        Value::Boolean(b) => out.write_all(if *b { &b"true"[..] } else { &b"false"[..] }),
        Value::Int16(v) => write!(out, "{v}"),
        Value::Int32(v) => write!(out, "{v}"),
        Value::Int64(v) => write!(out, "{v}"),
        Value::Int128(v) => write!(out, "{v}"),
        Value::Float64(f) => {
            write_f64(out, *f);
            Ok(())
        }
        Value::Decimal { value, scale, .. } => {
            decimal::write(out, *value, *scale);
            Ok(())
        }
        Value::Varchar(bytes) => {
            write_escaped(out, bytes);
            Ok(())
        }
        Value::Date(d) => {
            datetime::write_date(out, *d);
            Ok(())
        }
        Value::Timestamp(t) => {
            datetime::write_timestamp(out, *t);
            Ok(())
        }
    };
}

/// Appends one row: values separated by `\t`, then `\n`.
pub fn write_row(out: &mut Vec<u8>, row: &[Value]) {
    for (i, value) in row.iter().enumerate() {
        if i > 0 {
            out.push(b'\t');
        }
        write_value(out, value);
    }
    out.push(b'\n');
}

/// Appends the header line of `--format tsv-header`.
pub fn write_header(out: &mut Vec<u8>, names: &[&str]) {
    for (i, name) in names.iter().enumerate() {
        if i > 0 {
            out.push(b'\t');
        }
        write_escaped(out, name.as_bytes());
    }
    out.push(b'\n');
}

/// Appends every row of a chunk.
pub fn write_chunk(out: &mut Vec<u8>, chunk: &Chunk) {
    for i in 0..chunk.len {
        for (c, column) in chunk.columns.iter().enumerate() {
            if c > 0 {
                out.push(b'\t');
            }
            write_value(out, &column.get(i));
        }
        out.push(b'\n');
    }
}

/// Undoes the escapes of [`write_escaped`] (and `\'`); an unknown escape keeps the
/// backslash and a trailing lone backslash is an error.
pub fn unescape(field: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(field.len());
    let mut i = 0;
    while i < field.len() {
        let b = field[i];
        if b != b'\\' {
            out.push(b);
            i += 1;
            continue;
        }
        let Some(&next) = field.get(i + 1) else {
            return Err(VetaError::user("TSV field ends with a lone backslash"));
        };
        match next {
            b'\\' => out.push(b'\\'),
            b't' => out.push(b'\t'),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b'0' => out.push(0),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0C),
            b'\'' => out.push(b'\''),
            other => {
                out.push(b'\\');
                out.push(other);
            }
        }
        i += 2;
    }
    Ok(out)
}

/// Splits one line (without its `\n`) into fields; escaped tabs are `\t`, so every raw tab
/// is a separator.
pub fn split_fields(line: &[u8]) -> Vec<&[u8]> {
    line.split(|&b| b == b'\t').collect()
}

fn parse_int<T: std::str::FromStr>(field: &[u8], data_type: DataType) -> Result<T> {
    std::str::from_utf8(field)
        .ok()
        .and_then(|s| s.parse::<T>().ok())
        .ok_or_else(|| invalid(field, data_type))
}

fn invalid(field: &[u8], data_type: DataType) -> VetaError {
    VetaError::user(format!(
        "invalid {data_type} value in TSV: '{}'",
        String::from_utf8_lossy(field)
    ))
}

/// Parses one TSV field as a value of `data_type`; `\N` is NULL.
pub fn parse_value(field: &[u8], data_type: DataType) -> Result<Value> {
    if field == b"\\N" {
        return Ok(Value::Null);
    }
    Ok(match data_type {
        DataType::Boolean => match field {
            b"true" | b"1" => Value::Boolean(true),
            b"false" | b"0" => Value::Boolean(false),
            _ => return Err(invalid(field, data_type)),
        },
        DataType::Int16 => Value::Int16(parse_int(field, data_type)?),
        DataType::Int32 => Value::Int32(parse_int(field, data_type)?),
        DataType::Int64 => Value::Int64(parse_int(field, data_type)?),
        DataType::Int128 => Value::Int128(parse_int(field, data_type)?),
        DataType::Float64 => Value::Float64(parse_int(field, data_type)?),
        DataType::Decimal { precision, scale } => {
            let text = std::str::from_utf8(field).map_err(|_| invalid(field, data_type))?;
            Value::Decimal {
                value: decimal::parse(text, precision, scale)?,
                precision,
                scale,
            }
        }
        DataType::Varchar => Value::Varchar(unescape(field)?),
        DataType::Date => Value::Date(datetime::parse_date(field)?),
        DataType::Timestamp => Value::Timestamp(datetime::parse_timestamp(field)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &Value) -> String {
        let mut out = Vec::new();
        write_value(&mut out, value);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn escapes_and_null() {
        assert_eq!(text(&Value::Null), "\\N");
        assert_eq!(
            text(&Value::varchar("a\tb\nc\\d\re\0f\x08g\x0Ch'i")),
            "a\\tb\\nc\\\\d\\re\\0f\\bg\\fh'i"
        );
        assert_eq!(text(&Value::varchar("\\N")), "\\\\N");
        assert_eq!(text(&Value::varchar("")), "");
        let mut raw = Vec::new();
        write_value(&mut raw, &Value::Varchar(vec![0xFF, 0xC3, 0xA9]));
        assert_eq!(raw, vec![0xFF, 0xC3, 0xA9]);
    }

    #[test]
    fn scalars() {
        assert_eq!(text(&Value::Boolean(true)), "true");
        assert_eq!(text(&Value::Boolean(false)), "false");
        assert_eq!(text(&Value::Int16(-5)), "-5");
        assert_eq!(text(&Value::Int64(i64::MIN)), "-9223372036854775808");
        assert_eq!(text(&Value::Int128(i128::MAX)), i128::MAX.to_string());
        let dec = Value::Decimal {
            value: 5,
            precision: 3,
            scale: 2,
        };
        assert_eq!(text(&dec), "0.05");
        assert_eq!(text(&Value::Date(11_016)), "2000-02-29");
        assert_eq!(
            text(&Value::Timestamp(1_372_672_800)),
            "2013-07-01 10:00:00"
        );
    }

    #[test]
    fn floats_are_shortest_round_trip() {
        assert_eq!(text(&Value::Float64(0.1)), "0.1");
        assert_eq!(text(&Value::Float64(1.0)), "1");
        assert_eq!(text(&Value::Float64(-2.5)), "-2.5");
        assert_eq!(text(&Value::Float64(0.0)), "0");
        assert_eq!(text(&Value::Float64(1e300)), "1e300");
        assert_eq!(text(&Value::Float64(1.5e-7)), "1.5e-7");
        assert_eq!(text(&Value::Float64(f64::INFINITY)), "inf");
        assert_eq!(text(&Value::Float64(f64::NEG_INFINITY)), "-inf");
        assert_eq!(text(&Value::Float64(f64::NAN)), "nan");
        for f in [
            0.1 + 0.2,
            1.0 / 3.0,
            123456.789,
            1e-300,
            f64::MAX,
            f64::MIN_POSITIVE,
        ] {
            assert_eq!(text(&Value::Float64(f)).parse::<f64>().unwrap(), f);
        }
    }

    #[test]
    fn rows_chunks_and_header() {
        let mut out = Vec::new();
        write_row(
            &mut out,
            &[Value::Int64(1), Value::Null, Value::varchar("x y")],
        );
        assert_eq!(out, b"1\t\\N\tx y\n");
        let chunk = Chunk::from_rows(
            &[DataType::Int32, DataType::Varchar],
            &[
                vec![Value::Int32(1), Value::varchar("a\tb")],
                vec![Value::Null, Value::Null],
            ],
        )
        .unwrap();
        let mut out = Vec::new();
        write_header(&mut out, &["id", "name"]);
        write_chunk(&mut out, &chunk);
        assert_eq!(out, b"id\tname\n1\ta\\tb\n\\N\t\\N\n");
    }

    #[test]
    fn parse_inverts_write() {
        let cases = [
            (DataType::Boolean, Value::Boolean(true)),
            (DataType::Int16, Value::Int16(-300)),
            (DataType::Int32, Value::Int32(70_000)),
            (DataType::Int64, Value::Int64(i64::MAX)),
            (DataType::Float64, Value::Float64(0.1)),
            (
                DataType::Decimal {
                    precision: 15,
                    scale: 2,
                },
                Value::Decimal {
                    value: -1234,
                    precision: 15,
                    scale: 2,
                },
            ),
            (DataType::Varchar, Value::varchar("tab\there\\ \0 nul")),
            (DataType::Date, Value::Date(-1)),
            (DataType::Timestamp, Value::Timestamp(-1)),
            (DataType::Int64, Value::Null),
        ];
        for (data_type, value) in cases {
            let mut out = Vec::new();
            write_value(&mut out, &value);
            assert_eq!(parse_value(&out, data_type).unwrap(), value, "{data_type}");
        }
        assert_eq!(unescape(b"it\\'s").unwrap(), b"it's");
        assert_eq!(unescape(b"a\\qb").unwrap(), b"a\\qb");
        assert!(unescape(b"bad\\").is_err());
        assert!(parse_value(b"70000", DataType::Int16).is_err());
        assert!(parse_value(b"x", DataType::Boolean).is_err());
        assert_eq!(split_fields(b"a\t\\N\t"), vec![&b"a"[..], b"\\N", b""]);
    }
}
