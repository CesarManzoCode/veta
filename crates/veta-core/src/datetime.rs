//! Dates (days since 1970-01-01) and timestamps (seconds since 1970-01-01 00:00:00 UTC):
//! civil conversion, `YYYY-MM-DD[ HH:MM:SS]` parsing and formatting, `extract` and
//! `date_trunc` (contract §4, §10.3, §13.1).

use std::io::Write;

use crate::error::{Result, VetaError};

pub const SECONDS_PER_DAY: i64 = 86_400;

pub fn is_leap_year(year: i64) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

pub fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date, without validation
/// (H. Hinnant's `days_from_civil`).
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (month as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Civil date `(year, month, day)` of a day number (H. Hinnant's `civil_from_days`).
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// `Date` value of a validated civil date; `None` if the date does not exist or does not
/// fit `i32`.
pub fn date_from_ymd(year: i64, month: u32, day: u32) -> Option<i32> {
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    i32::try_from(days_from_civil(year, month, day)).ok()
}

fn invalid(kind: &str, text: &[u8]) -> VetaError {
    VetaError::user(format!(
        "invalid {kind}: '{}'",
        String::from_utf8_lossy(text)
    ))
}

fn digits(text: &[u8]) -> Option<u32> {
    if text.is_empty() || !text.iter().all(u8::is_ascii_digit) {
        return None;
    }
    text.iter().try_fold(0u32, |acc, &b| {
        acc.checked_mul(10)?.checked_add((b - b'0') as u32)
    })
}

fn parse_ymd(text: &[u8]) -> Option<i32> {
    if text.len() != 10 || text[4] != b'-' || text[7] != b'-' {
        return None;
    }
    let year = digits(&text[..4])?;
    let month = digits(&text[5..7])?;
    let day = digits(&text[8..10])?;
    date_from_ymd(year as i64, month, day)
}

/// Parses `YYYY-MM-DD`.
pub fn parse_date(text: &[u8]) -> Result<i32> {
    parse_ymd(text).ok_or_else(|| invalid("date", text))
}

/// Parses `YYYY-MM-DD HH:MM:SS` (or `T` as separator) or a bare `YYYY-MM-DD` (midnight).
pub fn parse_timestamp(text: &[u8]) -> Result<i64> {
    let err = || invalid("timestamp", text);
    let date = parse_ymd(text.get(..10).ok_or_else(err)?).ok_or_else(err)?;
    let mut seconds = date as i64 * SECONDS_PER_DAY;
    if text.len() == 10 {
        return Ok(seconds);
    }
    let time = &text[10..];
    if time.len() != 9
        || !(time[0] == b' ' || time[0] == b'T')
        || time[3] != b':'
        || time[6] != b':'
    {
        return Err(err());
    }
    let hour = digits(&time[1..3]).ok_or_else(err)?;
    let minute = digits(&time[4..6]).ok_or_else(err)?;
    let second = digits(&time[7..9]).ok_or_else(err)?;
    if hour > 23 || minute > 59 || second > 59 {
        return Err(err());
    }
    seconds += (hour * 3600 + minute * 60 + second) as i64;
    Ok(seconds)
}

fn write_ymd(out: &mut Vec<u8>, days: i64) {
    let (year, month, day) = civil_from_days(days);
    let _ = if (0..=9999).contains(&year) {
        write!(out, "{year:04}-{month:02}-{day:02}")
    } else {
        write!(out, "{year}-{month:02}-{day:02}")
    };
}

/// Appends `YYYY-MM-DD`.
pub fn write_date(out: &mut Vec<u8>, days: i32) {
    write_ymd(out, days as i64);
}

/// Appends `YYYY-MM-DD HH:MM:SS`.
pub fn write_timestamp(out: &mut Vec<u8>, seconds: i64) {
    let days = seconds.div_euclid(SECONDS_PER_DAY);
    let sod = seconds.rem_euclid(SECONDS_PER_DAY);
    write_ymd(out, days);
    let _ = write!(
        out,
        " {:02}:{:02}:{:02}",
        sod / 3600,
        sod % 3600 / 60,
        sod % 60
    );
}

pub fn format_date(days: i32) -> String {
    let mut out = Vec::with_capacity(10);
    write_date(&mut out, days);
    String::from_utf8(out).unwrap_or_default()
}

pub fn format_timestamp(seconds: i64) -> String {
    let mut out = Vec::with_capacity(19);
    write_timestamp(&mut out, seconds);
    String::from_utf8(out).unwrap_or_default()
}

/// Field of `extract(field FROM e)` (contract §10.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatePart {
    Year,
    Month,
    Day,
    Hour,
    Minute,
    Second,
}

impl DatePart {
    pub fn parse(name: &str) -> Result<DatePart> {
        match name.to_ascii_lowercase().as_str() {
            "year" => Ok(DatePart::Year),
            "month" => Ok(DatePart::Month),
            "day" => Ok(DatePart::Day),
            "hour" => Ok(DatePart::Hour),
            "minute" => Ok(DatePart::Minute),
            "second" => Ok(DatePart::Second),
            _ => Err(VetaError::unsupported(format!("extract({name} FROM …)"))),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            DatePart::Year => "year",
            DatePart::Month => "month",
            DatePart::Day => "day",
            DatePart::Hour => "hour",
            DatePart::Minute => "minute",
            DatePart::Second => "second",
        }
    }
}

/// `extract(part FROM date)`. A date has no time of day, so hour, minute and second are
/// rejected.
pub fn extract_date(part: DatePart, days: i32) -> Result<i64> {
    let (year, month, day) = civil_from_days(days as i64);
    match part {
        DatePart::Year => Ok(year),
        DatePart::Month => Ok(month as i64),
        DatePart::Day => Ok(day as i64),
        DatePart::Hour | DatePart::Minute | DatePart::Second => Err(VetaError::unsupported(
            format!("extract({} FROM DATE)", part.name()),
        )),
    }
}

/// `extract(part FROM timestamp)`.
pub fn extract_timestamp(part: DatePart, seconds: i64) -> i64 {
    let sod = seconds.rem_euclid(SECONDS_PER_DAY);
    match part {
        DatePart::Hour => sod / 3600,
        DatePart::Minute => sod % 3600 / 60,
        DatePart::Second => sod % 60,
        DatePart::Year | DatePart::Month | DatePart::Day => {
            let (year, month, day) = civil_from_days(seconds.div_euclid(SECONDS_PER_DAY));
            match part {
                DatePart::Year => year,
                DatePart::Month => month as i64,
                _ => day as i64,
            }
        }
    }
}

/// Unit of `DATE_TRUNC(unit, e)` (contract §10.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TruncUnit {
    Minute,
    Hour,
    Day,
    Month,
    Year,
}

impl TruncUnit {
    pub fn parse(name: &str) -> Result<TruncUnit> {
        match name.to_ascii_lowercase().as_str() {
            "minute" => Ok(TruncUnit::Minute),
            "hour" => Ok(TruncUnit::Hour),
            "day" => Ok(TruncUnit::Day),
            "month" => Ok(TruncUnit::Month),
            "year" => Ok(TruncUnit::Year),
            _ => Err(VetaError::unsupported(format!("DATE_TRUNC('{name}', …)"))),
        }
    }
}

/// `DATE_TRUNC` on a timestamp, in seconds.
pub fn date_trunc_timestamp(unit: TruncUnit, seconds: i64) -> i64 {
    match unit {
        TruncUnit::Minute => seconds - seconds.rem_euclid(60),
        TruncUnit::Hour => seconds - seconds.rem_euclid(3600),
        TruncUnit::Day => seconds - seconds.rem_euclid(SECONDS_PER_DAY),
        TruncUnit::Month | TruncUnit::Year => {
            date_trunc_days(unit, seconds.div_euclid(SECONDS_PER_DAY)) * SECONDS_PER_DAY
        }
    }
}

/// `DATE_TRUNC` on a date, in days; minute, hour and day leave a date unchanged.
pub fn date_trunc_date(unit: TruncUnit, days: i32) -> i32 {
    date_trunc_days(unit, days as i64) as i32
}

fn date_trunc_days(unit: TruncUnit, days: i64) -> i64 {
    match unit {
        TruncUnit::Minute | TruncUnit::Hour | TruncUnit::Day => days,
        TruncUnit::Month => {
            let (year, month, _) = civil_from_days(days);
            days_from_civil(year, month, 1)
        }
        TruncUnit::Year => {
            let (year, _, _) = civil_from_days(days);
            days_from_civil(year, 1, 1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_dates() {
        assert_eq!(parse_date(b"1970-01-01").unwrap(), 0);
        assert_eq!(parse_date(b"1969-12-31").unwrap(), -1);
        assert_eq!(parse_date(b"2000-02-29").unwrap(), 11_016);
        assert_eq!(parse_date(b"2000-03-01").unwrap(), 11_017);
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(-1), "1969-12-31");
        assert_eq!(format_date(11_016), "2000-02-29");
        assert_eq!(civil_from_days(-719_468), (0, 3, 1));
    }

    #[test]
    fn invalid_dates_are_user_errors() {
        for text in [
            &b"2001-02-29"[..],
            b"1900-02-29",
            b"2020-13-01",
            b"2020-00-10",
            b"2020-04-31",
            b"2020-1-01",
            b"2020/01/01",
            b"",
            b"abcd-ef-gh",
        ] {
            let err = parse_date(text).unwrap_err();
            assert_eq!(err.exit_code(), 1);
        }
    }

    #[test]
    fn civil_round_trip() {
        for days in (-800_000..800_000).step_by(97) {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
            assert!(d >= 1 && d <= days_in_month(y, m));
        }
    }

    #[test]
    fn timestamps() {
        assert_eq!(parse_timestamp(b"1970-01-01 00:00:00").unwrap(), 0);
        assert_eq!(
            parse_timestamp(b"2013-07-01 10:00:00").unwrap(),
            1_372_672_800
        );
        assert_eq!(
            parse_timestamp(b"2013-07-01T10:00:00").unwrap(),
            1_372_672_800
        );
        assert_eq!(parse_timestamp(b"2013-07-01").unwrap(), 1_372_636_800);
        assert_eq!(parse_timestamp(b"1969-12-31 23:59:59").unwrap(), -1);
        assert!(parse_timestamp(b"2013-07-01 24:00:00").is_err());
        assert!(parse_timestamp(b"2013-07-01 10:00").is_err());
        assert!(parse_timestamp(b"2013-07-01 10:00:00.5").is_err());
        assert_eq!(format_timestamp(-1), "1969-12-31 23:59:59");
        assert_eq!(format_timestamp(1_372_672_800), "2013-07-01 10:00:00");
        assert_eq!(format_timestamp(951_782_400 + 3_661), "2000-02-29 01:01:01");
    }

    #[test]
    fn extract_fields() {
        let ts = parse_timestamp(b"2000-02-29 13:45:07").unwrap();
        assert_eq!(extract_timestamp(DatePart::Year, ts), 2000);
        assert_eq!(extract_timestamp(DatePart::Month, ts), 2);
        assert_eq!(extract_timestamp(DatePart::Day, ts), 29);
        assert_eq!(extract_timestamp(DatePart::Hour, ts), 13);
        assert_eq!(extract_timestamp(DatePart::Minute, ts), 45);
        assert_eq!(extract_timestamp(DatePart::Second, ts), 7);
        assert_eq!(extract_timestamp(DatePart::Hour, -1), 23);
        assert_eq!(extract_date(DatePart::Year, -1).unwrap(), 1969);
        assert_eq!(extract_date(DatePart::Month, 11_016).unwrap(), 2);
        assert_eq!(extract_date(DatePart::Day, 11_016).unwrap(), 29);
        assert!(extract_date(DatePart::Hour, 0).is_err());
        assert_eq!(DatePart::parse("YEAR").unwrap(), DatePart::Year);
        assert!(DatePart::parse("week").is_err());
    }

    #[test]
    fn truncation() {
        let ts = parse_timestamp(b"2013-07-15 14:23:59").unwrap();
        let trunc = |unit| format_timestamp(date_trunc_timestamp(unit, ts));
        assert_eq!(trunc(TruncUnit::Minute), "2013-07-15 14:23:00");
        assert_eq!(trunc(TruncUnit::Hour), "2013-07-15 14:00:00");
        assert_eq!(trunc(TruncUnit::Day), "2013-07-15 00:00:00");
        assert_eq!(trunc(TruncUnit::Month), "2013-07-01 00:00:00");
        assert_eq!(trunc(TruncUnit::Year), "2013-01-01 00:00:00");
        assert_eq!(
            format_timestamp(date_trunc_timestamp(TruncUnit::Minute, -1)),
            "1969-12-31 23:59:00"
        );
        let day = parse_date(b"2000-02-29").unwrap();
        assert_eq!(
            format_date(date_trunc_date(TruncUnit::Month, day)),
            "2000-02-01"
        );
        assert_eq!(
            format_date(date_trunc_date(TruncUnit::Year, day)),
            "2000-01-01"
        );
        assert_eq!(date_trunc_date(TruncUnit::Day, day), day);
        assert_eq!(TruncUnit::parse("Minute").unwrap(), TruncUnit::Minute);
        assert!(TruncUnit::parse("week").is_err());
    }
}
