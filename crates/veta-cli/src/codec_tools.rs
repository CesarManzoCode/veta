//! `codec-roundtrip` and `codec-bench` (contract §13).
//!
//! A case is a codec applied to a column: every encoding of §8.1 that applies, `vlz` and
//! Snappy over the `Plain` bytes, and the whole block of §5.3, which also carries the
//! validity map. Values are compared bit by bit, so `NaN` and `-0.0` must survive too.

use std::fs;
use std::io::{self, BufWriter, Write};
use std::sync::atomic::{AtomicUsize, Ordering};

use veta_codec::{Compressor, Encoding, bench_compressor, decode, encode_with, snappy, vlz};
use veta_core::error::{Result, VetaError};
use veta_core::rand::Rng;
use veta_core::{DataType, Value, Vector, VectorData};
use veta_exec::{Database, ExecOptions, keep_worker_error};
use veta_storage::block::{block_bytes, read_block_exact};
use veta_storage::{PartReader, Snapshot, Table};

use crate::args::Parsed;
use crate::table_tools::read_or_detach;

/// Codec names of the cases of one column, in a fixed order, with their outcome.
type Outcomes = Vec<(&'static str, bool)>;

fn same_data(a: &VectorData, b: &VectorData) -> bool {
    match (a, b) {
        (VectorData::Float64(x), VectorData::Float64(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| p.to_bits() == q.to_bits())
        }
        _ => a == b,
    }
}

fn check_compressor(plain: &[u8], compress: fn(&[u8]) -> Vec<u8>, is_vlz: bool) -> bool {
    let compressed = compress(plain);
    let restored = if is_vlz {
        vlz::decompress(&compressed)
    } else {
        snappy::decompress(&compressed)
    };
    restored.is_ok_and(|bytes| bytes == plain)
}

/// Runs every case on `v`.
fn check_vector(v: &Vector) -> Outcomes {
    let data_type = v.data_type();
    let mut out = Vec::new();
    for encoding in Encoding::ALL {
        if encoding.applies_to(&data_type) {
            let ok = encode_with(v, encoding)
                .and_then(|bytes| decode(encoding, &bytes, &data_type, v.len()))
                .is_ok_and(|decoded| same_data(&decoded.data, &v.data));
            out.push((encoding.name(), ok));
        }
    }
    let plain = encode_with(v, Encoding::Plain);
    out.push((
        "vlz",
        plain
            .as_ref()
            .is_ok_and(|bytes| check_compressor(bytes, vlz::compress, true)),
    ));
    out.push((
        "snappy",
        plain
            .as_ref()
            .is_ok_and(|bytes| check_compressor(bytes, snappy::compress, false)),
    ));
    let block = block_bytes(v)
        .and_then(|bytes| read_block_exact(&bytes, data_type, true, v.len(), "roundtrip"))
        .is_ok_and(|read| {
            same_data(&read.data, &v.data)
                && (0..v.len()).all(|i| read.is_valid(i) == v.is_valid(i))
        });
    out.push(("block", block));
    out
}

const RANDOM_TYPES: [DataType; 10] = [
    DataType::Boolean,
    DataType::Int16,
    DataType::Int32,
    DataType::Int64,
    DataType::Float64,
    DataType::Decimal {
        precision: 10,
        scale: 2,
    },
    DataType::Decimal {
        precision: 30,
        scale: 4,
    },
    DataType::Varchar,
    DataType::Date,
    DataType::Timestamp,
];

/// A length around the sizes the formats care about: multiples of 128 and 8192 and their
/// neighbours, or a random one.
fn random_len(rng: &mut Rng) -> usize {
    const AROUND: [usize; 10] = [1, 7, 127, 128, 129, 256, 1000, 8191, 8192, 300];
    if rng.next_bool(0.5) {
        AROUND[rng.below(AROUND.len() as u64) as usize]
    } else {
        1 + rng.below(8192) as usize
    }
}

fn random_value(data_type: DataType, x: i64) -> Value {
    match data_type {
        DataType::Boolean => Value::Boolean(x & 1 == 1),
        DataType::Int16 => Value::Int16(x as i16),
        DataType::Int32 => Value::Int32(x as i32),
        DataType::Int64 | DataType::Int128 => Value::Int64(x),
        DataType::Float64 => Value::Float64(if x % 97 == 0 {
            f64::NAN
        } else {
            x as f64 * 0.5
        }),
        DataType::Decimal { precision, scale } => {
            let bound = 10i128.pow(precision.min(18) as u32);
            Value::Decimal {
                value: x as i128 % bound,
                precision,
                scale,
            }
        }
        DataType::Varchar => Value::varchar(format!("s{}", x.rem_euclid(100_000))),
        DataType::Date => Value::Date(x.rem_euclid(40_000) as i32),
        DataType::Timestamp => Value::Timestamp(x.rem_euclid(4_000_000_000)),
    }
}

/// A column of type number `index` with a random length, share of NULLs and distribution:
/// uniform, a small range, a constant, increasing or in runs.
fn random_column(rng: &mut Rng, index: usize) -> Vector {
    let data_type = RANDOM_TYPES[index % RANDOM_TYPES.len()];
    let len = random_len(rng);
    let null_share = [0.0, 0.0, 0.1, 0.5, 1.0][rng.below(5) as usize];
    let shape = rng.below(5);
    let start = rng.next_u64() as i64;
    let mut run_value = rng.range_i64(-1000, 1000);
    let mut vector = Vector::with_capacity(data_type, len);
    for i in 0..len {
        let x = match shape {
            0 => rng.next_u64() as i64,
            1 => rng.range_i64(-50, 50),
            2 => start,
            3 => (start / 2).wrapping_add(i as i64 * 3),
            _ => {
                if rng.next_bool(0.02) {
                    run_value = rng.range_i64(-1000, 1000);
                }
                run_value
            }
        };
        let value = if rng.next_bool(null_share) {
            Value::Null
        } else {
            random_value(data_type, x)
        };
        vector
            .push(&value)
            .expect("the value was made for this type");
    }
    vector
}

/// Combines the outcomes of a column over its blocks: a case fails if any block fails.
fn fold_outcomes(total: &mut Outcomes, block: Outcomes) {
    if total.is_empty() {
        *total = block;
        return;
    }
    for (slot, (_, ok)) in total.iter_mut().zip(block) {
        slot.1 &= ok;
    }
}

/// The cases of one stored column over every block of the snapshot.
fn check_column(table: &Table, parts: &[PartReader], column: usize) -> Result<Outcomes> {
    let mut total = Vec::new();
    for part in parts {
        for granule in 0..part.granules() {
            fold_outcomes(
                &mut total,
                check_vector(&read_or_detach(table, part, column, granule)?),
            );
        }
    }
    Ok(total)
}

/// Checks every column on the worker threads; the results come back in column order.
fn check_table(table: &Table, parts: &[PartReader], columns: usize) -> Result<Vec<Outcomes>> {
    let next = AtomicUsize::new(0);
    let next = &next;
    let workers = ExecOptions::default_threads().min(columns).max(1);
    let joined: Vec<Result<Vec<(usize, Outcomes)>>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(move || -> Result<Vec<(usize, Outcomes)>> {
                    let mut done = Vec::new();
                    loop {
                        let column = next.fetch_add(1, Ordering::Relaxed);
                        if column >= columns {
                            return Ok(done);
                        }
                        done.push((column, check_column(table, parts, column)?));
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| {
                handle
                    .join()
                    .unwrap_or_else(|_| Err(VetaError::internal("worker thread panicked")))
            })
            .collect()
    });
    let mut all = Vec::with_capacity(columns);
    let mut failure = None;
    for done in joined {
        match done {
            Ok(done) => all.extend(done),
            Err(err) => keep_worker_error(&mut failure, err),
        }
    }
    if let Some(err) = failure {
        return Err(err);
    }
    all.sort_by_key(|entry| entry.0);
    Ok(all.into_iter().map(|entry| entry.1).collect())
}

/// Prints one line per case and counts `(cases, failures)`.
fn report(
    out: &mut impl Write,
    column: &str,
    outcomes: &Outcomes,
    counts: &mut (u64, u64),
) -> Result<()> {
    for &(codec, ok) in outcomes {
        counts.0 += 1;
        if ok {
            writeln!(out, "OK {codec} {column}")?;
        } else {
            counts.1 += 1;
            writeln!(out, "FAIL {codec} {column}")?;
        }
    }
    Ok(())
}

/// `codec-roundtrip [--db D --table T] [--random N --seed S]`.
pub(crate) fn cmd_codec_roundtrip(args: &[String]) -> Result<()> {
    let parsed = Parsed::new(args, &["--db", "--table", "--random", "--seed"], &[], &[])?;
    parsed.no_positional()?;
    let random = parsed.parsed::<u64>("--random")?;
    let seed = parsed.parsed::<u64>("--seed")?.unwrap_or(0);
    let table = match (parsed.value("--db"), parsed.value("--table")) {
        (Some(db), Some(table)) => Some((db, table)),
        (None, None) => None,
        _ => return Err(VetaError::user("--db and --table go together")),
    };
    if table.is_none() && random.is_none() {
        return Err(VetaError::user(
            "codec-roundtrip needs --db and --table, or --random",
        ));
    }

    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    let mut counts = (0, 0);
    if let Some((db, name)) = table {
        let table = Database::open(db).open_table(name)?;
        let snapshot = Snapshot::take(&table, None)?;
        for detached in snapshot.detached() {
            eprintln!("{}", detached.message());
        }
        let fields = &table.schema().fields;
        let outcomes = check_table(&table, snapshot.parts(), fields.len())?;
        for (field, column) in fields.iter().zip(&outcomes) {
            report(&mut out, &field.name, column, &mut counts)?;
        }
    }
    if let Some(columns) = random {
        let mut rng = Rng::new(seed);
        for index in 0..columns {
            let vector = random_column(&mut rng, index as usize);
            let name = format!("random{index}:{}", vector.data_type());
            report(&mut out, &name, &check_vector(&vector), &mut counts)?;
        }
    }
    writeln!(
        out,
        "codec-roundtrip: {} cases, {} failures",
        counts.0, counts.1
    )?;
    out.flush()?;
    if counts.1 > 0 {
        return Err(VetaError::internal(format!(
            "codec-roundtrip: {} failures",
            counts.1
        )));
    }
    Ok(())
}

/// `s` as a JSON string.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `codec-bench --compressor vlz --file F… [--iterations N]`.
pub(crate) fn cmd_codec_bench(args: &[String]) -> Result<()> {
    let parsed = Parsed::new(args, &["--compressor", "--iterations"], &["--file"], &[])?;
    parsed.no_positional()?;
    let compressor = Compressor::from_name(parsed.required("--compressor")?)?;
    let iterations = parsed.parsed::<usize>("--iterations")?.unwrap_or(3);
    let files = parsed.values("--file");
    if files.is_empty() {
        return Err(VetaError::user("codec-bench needs --file"));
    }
    for file in files {
        let data = fs::read(file).map_err(|e| VetaError::Io(format!("{file}: {e}")))?;
        let result = bench_compressor(compressor, &data, iterations)?;
        println!(
            "{{\"file\":{},\"raw_bytes\":{},\"compressed_bytes\":{},\"compress_mb_s\":{},\"decompress_mb_s\":{}}}",
            json_string(file),
            result.raw_bytes,
            result.compressed_bytes,
            result.compress_mb_s,
            result.decompress_mb_s
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_columns_cover_every_type_and_pass() {
        let mut rng = Rng::new(11);
        for index in 0..60 {
            let vector = random_column(&mut rng, index);
            assert_eq!(vector.data_type(), RANDOM_TYPES[index % RANDOM_TYPES.len()]);
            assert!(!vector.is_empty());
            for (codec, ok) in check_vector(&vector) {
                assert!(ok, "{codec} on column {index} ({})", vector.data_type());
            }
        }
    }

    #[test]
    fn a_wrong_result_fails_its_case() {
        assert!(!same_data(
            &VectorData::Float64(vec![0.0]),
            &VectorData::Float64(vec![-0.0])
        ));
        assert!(same_data(
            &VectorData::Float64(vec![f64::NAN]),
            &VectorData::Float64(vec![f64::NAN])
        ));
        let mut total: Outcomes = vec![("a", true), ("b", true)];
        fold_outcomes(&mut total, vec![("a", true), ("b", false)]);
        fold_outcomes(&mut total, vec![("a", true), ("b", true)]);
        assert_eq!(total, vec![("a", true), ("b", false)]);
    }

    #[test]
    fn reports_count_cases_and_failures() {
        let mut out = Vec::new();
        let mut counts = (0, 0);
        report(&mut out, "c", &vec![("x", true), ("y", false)], &mut counts).unwrap();
        assert_eq!(counts, (2, 1));
        assert_eq!(String::from_utf8(out).unwrap(), "OK x c\nFAIL y c\n");
    }

    #[test]
    fn json_strings_are_escaped() {
        assert_eq!(json_string("a\"b\\c\n"), "\"a\\\"b\\\\c\\u000a\"");
    }
}
