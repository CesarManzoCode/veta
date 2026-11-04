//! Scalar functions of §10.3 over vectors: `extract`, `length`/`STRLEN`, `substring`,
//! `DATE_TRUNC` and `REGEXP_REPLACE`.
//!
//! Every kernel matches the input type once and then runs a loop over the slice. NULL
//! inputs give NULL outputs; the value buffer of a NULL slot holds a default.

use veta_core::datetime::{self, DatePart, TruncUnit};
use veta_core::{Bitmap, Result, Vector, VectorData, VetaError};

use crate::regex::{Cache, Regex, Replacement};

/// An integer argument: a constant (`None` = NULL) or an `Int64` vector.
#[derive(Debug, Clone, Copy)]
pub enum IntArg<'a> {
    Const(Option<i64>),
    Vector(&'a Vector),
}

/// A string argument: a constant (`None` = NULL) or a `Varchar` vector.
#[derive(Debug, Clone, Copy)]
pub enum StrArg<'a> {
    Const(Option<&'a [u8]>),
    Vector(&'a Vector),
}

fn type_error(func: &str, v: &Vector) -> VetaError {
    VetaError::internal(format!("{func} got a {} vector", v.data_type()))
}

/// Builds a `Varchar` vector from a buffer filled row by row.
pub(crate) struct StringBuilder {
    offsets: Vec<u32>,
    bytes: Vec<u8>,
}

impl StringBuilder {
    pub(crate) fn with_capacity(rows: usize, bytes: usize) -> StringBuilder {
        let mut offsets = Vec::with_capacity(rows + 1);
        offsets.push(0);
        StringBuilder {
            offsets,
            bytes: Vec::with_capacity(bytes),
        }
    }

    pub(crate) fn bytes_mut(&mut self) -> &mut Vec<u8> {
        &mut self.bytes
    }

    /// Ends the current row (whatever was appended to `bytes_mut` since the last one).
    pub(crate) fn finish_row(&mut self) -> Result<()> {
        let end = u32::try_from(self.bytes.len())
            .map_err(|_| VetaError::internal("string vector exceeds 4 GiB"))?;
        self.offsets.push(end);
        Ok(())
    }

    pub(crate) fn push(&mut self, s: &[u8]) -> Result<()> {
        self.bytes.extend_from_slice(s);
        self.finish_row()
    }

    pub(crate) fn finish(self) -> VectorData {
        VectorData::Varchar {
            offsets: self.offsets,
            bytes: self.bytes,
        }
    }
}

/// `length(s)` / `STRLEN(s)`: bytes, as `Int64`.
pub fn length(v: &Vector) -> Result<Vector> {
    let VectorData::Varchar { offsets, .. } = &v.data else {
        return Err(type_error("length", v));
    };
    let values = offsets.windows(2).map(|w| (w[1] - w[0]) as i64).collect();
    Vector::new(VectorData::Int64(values), v.validity.clone())
}

/// `extract(part FROM e)` of a `Date` or `Timestamp`, as `Int64`.
pub fn extract(part: DatePart, v: &Vector) -> Result<Vector> {
    let values: Vec<i64> = match &v.data {
        VectorData::Timestamp(ts) => match part {
            DatePart::Hour => ts
                .iter()
                .map(|&t| t.rem_euclid(datetime::SECONDS_PER_DAY) / 3600)
                .collect(),
            DatePart::Minute => ts.iter().map(|&t| t.rem_euclid(3600) / 60).collect(),
            DatePart::Second => ts.iter().map(|&t| t.rem_euclid(60)).collect(),
            _ => ts
                .iter()
                .map(|&t| datetime::extract_timestamp(part, t))
                .collect(),
        },
        VectorData::Date(days) => {
            if matches!(part, DatePart::Hour | DatePart::Minute | DatePart::Second) {
                return Err(VetaError::unsupported(format!(
                    "extract({} FROM DATE)",
                    part.name()
                )));
            }
            days.iter()
                .map(|&d| datetime::extract_date(part, d).unwrap_or(0))
                .collect()
        }
        _ => return Err(type_error("extract", v)),
    };
    Vector::new(VectorData::Int64(values), v.validity.clone())
}

/// `DATE_TRUNC(unit, e)`; keeps the input type.
pub fn date_trunc(unit: TruncUnit, v: &Vector) -> Result<Vector> {
    let data = match &v.data {
        VectorData::Timestamp(ts) => VectorData::Timestamp(match unit {
            TruncUnit::Minute => ts.iter().map(|&t| t - t.rem_euclid(60)).collect(),
            TruncUnit::Hour => ts.iter().map(|&t| t - t.rem_euclid(3600)).collect(),
            TruncUnit::Day => ts
                .iter()
                .map(|&t| t - t.rem_euclid(datetime::SECONDS_PER_DAY))
                .collect(),
            TruncUnit::Month | TruncUnit::Year => ts
                .iter()
                .map(|&t| datetime::date_trunc_timestamp(unit, t))
                .collect(),
        }),
        VectorData::Date(days) => VectorData::Date(match unit {
            TruncUnit::Minute | TruncUnit::Hour | TruncUnit::Day => days.clone(),
            TruncUnit::Month | TruncUnit::Year => days
                .iter()
                .map(|&d| datetime::date_trunc_date(unit, d))
                .collect(),
        }),
        _ => return Err(type_error("date_trunc", v)),
    };
    Vector::new(data, v.validity.clone())
}

/// Byte range of `substring(s, from[, length])` over a string of `size` bytes, with the
/// DuckDB rules applied to bytes: positions are 1-based, a negative `from` counts from the
/// end, `from = 0` starts one position before the first byte, and a negative `length`
/// goes backwards.
pub fn substring_range(size: usize, from: i64, length: Option<i64>) -> (usize, usize) {
    let size = size as i64;
    let mut length = length.unwrap_or(i64::MAX);
    if length == 0 {
        return (0, 0);
    }
    let start = if from > 0 {
        size.min(from - 1)
    } else if from < 0 {
        (size.saturating_add(from)).max(0)
    } else {
        if length > 0 {
            length -= 1;
            if length == 0 {
                return (0, 0);
            }
        }
        0
    };
    let (start, end) = if length > 0 {
        (start, size.min(start.saturating_add(length)))
    } else {
        ((start.saturating_add(length)).max(0), start)
    };
    (start as usize, end.max(start) as usize)
}

fn int_at(arg: IntArg<'_>, i: usize) -> Result<Option<i64>> {
    match arg {
        IntArg::Const(c) => Ok(c),
        IntArg::Vector(v) => match &v.data {
            VectorData::Int64(x) => Ok(v.is_valid(i).then(|| x[i])),
            _ => Err(type_error("substring", v)),
        },
    }
}

/// `substring(s, from[, length])` in bytes, base 1 (§4).
pub fn substring(s: &Vector, from: IntArg<'_>, length: Option<IntArg<'_>>) -> Result<Vector> {
    let VectorData::Varchar { offsets, bytes } = &s.data else {
        return Err(type_error("substring", s));
    };
    let n = s.len();
    // Constant positions: one loop without per-row argument lookups.
    if let (IntArg::Const(Some(f)), None | Some(IntArg::Const(Some(_)))) = (from, length) {
        let len = match length {
            Some(IntArg::Const(l)) => l,
            _ => None,
        };
        let mut out = StringBuilder::with_capacity(n, bytes.len());
        for w in offsets.windows(2) {
            let row = &bytes[w[0] as usize..w[1] as usize];
            let (a, b) = substring_range(row.len(), f, len);
            out.push(&row[a..b])?;
        }
        return Vector::new(out.finish(), s.validity.clone());
    }
    let mut out = StringBuilder::with_capacity(n, bytes.len());
    let mut validity = Bitmap::new_set(n);
    for i in 0..n {
        let row = &bytes[offsets[i] as usize..offsets[i + 1] as usize];
        let f = int_at(from, i)?;
        let l = match length {
            Some(arg) => int_at(arg, i)?.map(Some),
            None => Some(None),
        };
        match (s.is_valid(i), f, l) {
            (true, Some(f), Some(l)) => {
                let (a, b) = substring_range(row.len(), f, l);
                out.push(&row[a..b])?;
            }
            _ => {
                validity.set(i, false);
                out.push(b"")?;
            }
        }
    }
    let validity = (!validity.all_set()).then_some(validity);
    Vector::new(out.finish(), validity)
}

/// `REGEXP_REPLACE` with a pattern and a replacement compiled once per query.
#[derive(Debug)]
pub struct RegexpReplace {
    regex: Regex,
    replacement: Replacement,
}

impl RegexpReplace {
    pub fn new(pattern: &[u8], replacement: &[u8]) -> Result<RegexpReplace> {
        let regex = Regex::new(pattern)?;
        let replacement = Replacement::new(replacement, &regex)?;
        Ok(RegexpReplace { regex, replacement })
    }

    /// Replaces every match in every row of a `Varchar` vector.
    pub fn apply(&self, v: &Vector) -> Result<Vector> {
        let VectorData::Varchar { offsets, bytes } = &v.data else {
            return Err(type_error("regexp_replace", v));
        };
        let mut cache = Cache::default();
        let mut out = StringBuilder::with_capacity(v.len(), bytes.len());
        for (i, w) in offsets.windows(2).enumerate() {
            if v.is_valid(i) {
                let row = &bytes[w[0] as usize..w[1] as usize];
                self.regex
                    .replace_all_into(row, &self.replacement, out.bytes_mut(), &mut cache);
            }
            out.finish_row()?;
        }
        Vector::new(out.finish(), v.validity.clone())
    }

    /// Replaces in one string.
    pub fn apply_one(&self, s: &[u8]) -> Vec<u8> {
        self.regex.replace_all(s, &self.replacement)
    }
}

fn str_at<'a>(arg: StrArg<'a>, i: usize) -> Result<Option<&'a [u8]>> {
    match arg {
        StrArg::Const(c) => Ok(c),
        StrArg::Vector(v) => {
            if !v.is_valid(i) {
                return Ok(None);
            }
            v.data
                .str_at(i)
                .map(Some)
                .ok_or_else(|| type_error("regexp_replace", v))
        }
    }
}

/// `REGEXP_REPLACE` whose pattern or replacement varies by row: each distinct pair is
/// compiled when it first appears in a run of rows.
pub fn regexp_replace(s: &Vector, pattern: StrArg<'_>, replacement: StrArg<'_>) -> Result<Vector> {
    if let (StrArg::Const(p), StrArg::Const(r)) = (pattern, replacement) {
        return match (p, r) {
            (Some(p), Some(r)) => RegexpReplace::new(p, r)?.apply(s),
            _ => Ok(Vector::nulls(veta_core::DataType::Varchar, s.len())),
        };
    }
    let n = s.len();
    let mut out = StringBuilder::with_capacity(n, 0);
    let mut validity = Bitmap::new_set(n);
    let mut last: Option<(Vec<u8>, Vec<u8>, RegexpReplace)> = None;
    let mut cache = Cache::default();
    for i in 0..n {
        let text = if s.is_valid(i) {
            s.data.str_at(i)
        } else {
            None
        };
        match (text, str_at(pattern, i)?, str_at(replacement, i)?) {
            (Some(text), Some(p), Some(r)) => {
                let reuse = matches!(&last, Some((lp, lr, _)) if lp == p && lr == r);
                if !reuse {
                    last = Some((p.to_vec(), r.to_vec(), RegexpReplace::new(p, r)?));
                }
                if let Some((_, _, rr)) = &last {
                    rr.regex
                        .replace_all_into(text, &rr.replacement, out.bytes_mut(), &mut cache);
                }
            }
            _ => validity.set(i, false),
        }
        out.finish_row()?;
    }
    let validity = (!validity.all_set()).then_some(validity);
    Vector::new(out.finish(), validity)
}

#[cfg(test)]
mod tests {
    use super::*;
    use veta_core::datetime::{parse_date, parse_timestamp};
    use veta_core::{DataType, Value};

    fn strings(values: &[Option<&str>]) -> Vector {
        let values: Vec<Value> = values
            .iter()
            .map(|v| v.map_or(Value::Null, Value::varchar))
            .collect();
        Vector::from_values(DataType::Varchar, &values).unwrap()
    }

    fn texts(v: &Vector) -> Vec<Option<String>> {
        (0..v.len())
            .map(|i| match v.get(i) {
                Value::Varchar(b) => Some(String::from_utf8(b).unwrap()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn length_counts_bytes() {
        let v = strings(&[Some("abc"), Some(""), None, Some("ñ")]);
        let out = length(&v).unwrap();
        assert_eq!(
            (0..4).map(|i| out.get(i)).collect::<Vec<_>>(),
            vec![
                Value::Int64(3),
                Value::Int64(0),
                Value::Null,
                Value::Int64(2)
            ]
        );
    }

    #[test]
    fn extract_of_known_timestamps() {
        let t = parse_timestamp(b"2013-07-15 13:45:59").unwrap();
        let before_epoch = parse_timestamp(b"1969-12-31 23:59:58").unwrap();
        let v = Vector::from_values(
            DataType::Timestamp,
            &[
                Value::Timestamp(t),
                Value::Timestamp(before_epoch),
                Value::Null,
            ],
        )
        .unwrap();
        let get = |part| {
            let out = extract(part, &v).unwrap();
            (0..3).map(|i| out.get(i)).collect::<Vec<_>>()
        };
        assert_eq!(
            get(DatePart::Year),
            vec![Value::Int64(2013), Value::Int64(1969), Value::Null]
        );
        assert_eq!(
            get(DatePart::Month)[..2],
            [Value::Int64(7), Value::Int64(12)]
        );
        assert_eq!(
            get(DatePart::Day)[..2],
            [Value::Int64(15), Value::Int64(31)]
        );
        assert_eq!(
            get(DatePart::Hour)[..2],
            [Value::Int64(13), Value::Int64(23)]
        );
        assert_eq!(
            get(DatePart::Minute)[..2],
            [Value::Int64(45), Value::Int64(59)]
        );
        assert_eq!(
            get(DatePart::Second)[..2],
            [Value::Int64(59), Value::Int64(58)]
        );

        let d = Vector::from_values(
            DataType::Date,
            &[Value::Date(parse_date(b"2000-02-29").unwrap())],
        )
        .unwrap();
        assert_eq!(
            extract(DatePart::Year, &d).unwrap().get(0),
            Value::Int64(2000)
        );
        assert_eq!(extract(DatePart::Day, &d).unwrap().get(0), Value::Int64(29));
        assert!(extract(DatePart::Hour, &d).is_err());
    }

    #[test]
    fn date_trunc_of_known_timestamps() {
        let ts = |s: &str| parse_timestamp(s.as_bytes()).unwrap();
        let v = Vector::from_values(
            DataType::Timestamp,
            &[
                Value::Timestamp(ts("2013-07-15 13:45:59")),
                Value::Timestamp(ts("1969-12-31 23:59:58")),
            ],
        )
        .unwrap();
        let get = |unit| {
            let out = date_trunc(unit, &v).unwrap();
            [out.get(0), out.get(1)]
        };
        assert_eq!(
            get(TruncUnit::Minute),
            [
                Value::Timestamp(ts("2013-07-15 13:45:00")),
                Value::Timestamp(ts("1969-12-31 23:59:00"))
            ]
        );
        assert_eq!(
            get(TruncUnit::Hour)[0],
            Value::Timestamp(ts("2013-07-15 13:00:00"))
        );
        assert_eq!(
            get(TruncUnit::Day)[0],
            Value::Timestamp(ts("2013-07-15 00:00:00"))
        );
        assert_eq!(
            get(TruncUnit::Day)[1],
            Value::Timestamp(ts("1969-12-31 00:00:00"))
        );
        assert_eq!(
            get(TruncUnit::Month)[0],
            Value::Timestamp(ts("2013-07-01 00:00:00"))
        );
        assert_eq!(
            get(TruncUnit::Year)[0],
            Value::Timestamp(ts("2013-01-01 00:00:00"))
        );
        assert_eq!(
            get(TruncUnit::Year)[1],
            Value::Timestamp(ts("1969-01-01 00:00:00"))
        );

        let d = Vector::from_values(
            DataType::Date,
            &[Value::Date(parse_date(b"2013-07-15").unwrap())],
        )
        .unwrap();
        assert_eq!(
            date_trunc(TruncUnit::Month, &d).unwrap().get(0),
            Value::Date(parse_date(b"2013-07-01").unwrap())
        );
        assert_eq!(
            date_trunc(TruncUnit::Hour, &d).unwrap().get(0),
            Value::Date(parse_date(b"2013-07-15").unwrap())
        );
    }

    #[test]
    fn substring_edges() {
        let r = |from, len| {
            let (a, b) = substring_range(5, from, len);
            &"hello"[a..b]
        };
        assert_eq!(r(1, Some(2)), "he");
        assert_eq!(r(2, None), "ello");
        assert_eq!(r(5, Some(10)), "o");
        assert_eq!(r(6, Some(1)), "");
        assert_eq!(r(100, None), "");
        assert_eq!(r(0, Some(2)), "h");
        assert_eq!(r(0, Some(1)), "");
        assert_eq!(r(0, None), "hello");
        assert_eq!(r(-3, Some(2)), "ll");
        assert_eq!(r(-10, Some(3)), "hel");
        assert_eq!(r(1, Some(0)), "");
        assert_eq!(r(3, Some(-2)), "he");
        assert_eq!(r(1, Some(i64::MAX)), "hello");
        assert_eq!(r(i64::MIN, Some(2)), "he");
        assert_eq!(substring_range(0, 1, Some(3)), (0, 0));

        let v = strings(&[Some("13-555"), Some("ab"), None, Some("")]);
        let out = substring(&v, IntArg::Const(Some(1)), Some(IntArg::Const(Some(2)))).unwrap();
        assert_eq!(
            texts(&out),
            vec![Some("13".into()), Some("ab".into()), None, Some("".into())]
        );
        // Bytes, not characters: the second byte of `ñ` is cut.
        let v = strings(&[Some("ñx")]);
        let out = substring(&v, IntArg::Const(Some(2)), None).unwrap();
        assert_eq!(out.get(0), Value::Varchar(vec![0xB1, b'x']));
        // Per-row positions, with a NULL position.
        let v = strings(&[Some("hello"), Some("world")]);
        let from = Vector::from_values(DataType::Int64, &[Value::Int64(2), Value::Null]).unwrap();
        let out = substring(&v, IntArg::Vector(&from), Some(IntArg::Const(Some(3)))).unwrap();
        assert_eq!(texts(&out), vec![Some("ell".into()), None]);
        let out = substring(&v, IntArg::Const(None), None).unwrap();
        assert_eq!(texts(&out), vec![None, None]);
    }

    #[test]
    fn regexp_replace_over_vectors() {
        let v = strings(&[
            Some("https://www.google.com/search?q=1"),
            Some("http://yandex.ru/"),
            None,
            Some("not a url"),
            Some("https://a.b/x\ny"),
        ]);
        let rr = RegexpReplace::new(br"^https?://(?:www\.)?([^/]+)/.*$", br"\1").unwrap();
        assert_eq!(
            texts(&rr.apply(&v).unwrap()),
            vec![
                Some("google.com".into()),
                Some("yandex.ru".into()),
                None,
                Some("not a url".into()),
                Some("a.b".into()),
            ]
        );
        let patterns = strings(&[Some("o"), Some("o"), Some("x"), Some("t"), None]);
        let out = regexp_replace(&v, StrArg::Vector(&patterns), StrArg::Const(Some(b"0"))).unwrap();
        let got = texts(&out);
        assert_eq!(got[0].as_deref(), Some("https://www.g00gle.c0m/search?q=1"));
        assert_eq!(got[1].as_deref(), Some("http://yandex.ru/"));
        assert_eq!(got[3].as_deref(), Some("no0 a url"));
        assert_eq!(got[2], None);
        assert_eq!(got[4], None);
        assert!(RegexpReplace::new(b"(", b"").is_err());
        assert!(regexp_replace(&v, StrArg::Const(Some(b"a{")), StrArg::Const(Some(b""))).is_err());
        let nulls = regexp_replace(&v, StrArg::Const(None), StrArg::Const(Some(b""))).unwrap();
        assert_eq!(nulls.null_count(), 5);
    }
}
