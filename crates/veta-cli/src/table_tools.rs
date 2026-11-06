//! Subcommands that work on the parts of a table: `recover`, `check`, `parts` and
//! `dump-columns` (contract §6.4, §6.5, §13).

use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::path::Path;

use veta_core::error::{Result, VetaError};
use veta_core::{Vector, VectorData};
use veta_exec::Database;
use veta_storage::{
    DetachedPart, PartReader, Snapshot, Table, check_table, detach_corrupt, recover,
};

use crate::args::Parsed;

/// `recover --db D [--table T]`: §6.5 on one table or on all of them.
pub(crate) fn cmd_recover(args: &[String]) -> Result<()> {
    let parsed = Parsed::new(args, &["--db", "--table"], &[], &[])?;
    parsed.no_positional()?;
    let db = Database::open(parsed.required("--db")?);
    let names = match parsed.value("--table") {
        Some(table) => vec![table.to_string()],
        None => db.table_names()?,
    };
    for name in names {
        let table = db.open_table(&name)?;
        let report = recover(&table)?;
        for detached in &report.detached {
            eprintln!("{}", detached.message());
        }
        println!(
            "recover: {name}: {} tmp removed, {} deletions finished, {} covered parts removed, {} parts detached",
            report.removed_tmp.len(),
            report.removed_delete_tmp.len(),
            report.removed_covered.len(),
            report.detached.len()
        );
    }
    Ok(())
}

/// Reads a block for a command that goes through the rows of a table. A corrupt one detaches
/// its part before the command fails with exit 3 (§6.4), as in a query.
pub(crate) fn read_or_detach(
    table: &Table,
    part: &PartReader,
    column: usize,
    granule: usize,
) -> Result<Vector> {
    part.read_granule(column, granule).inspect_err(|err| {
        if let Err(failure) = detach_corrupt(table.dir(), err) {
            eprintln!("veta: {failure}");
        }
    })
}

/// `check --db D --table T`: verifies every block of every part, detaches the corrupt ones
/// and exits with 3 if there was any (§6.4).
pub(crate) fn cmd_check(args: &[String]) -> Result<()> {
    let parsed = Parsed::new(args, &["--db", "--table"], &[], &[])?;
    parsed.no_positional()?;
    let db = Database::open(parsed.required("--db")?);
    let table = db.open_table(parsed.required("--table")?)?;
    let report = check_table(&table)?;
    for part in &report.broken {
        eprintln!("{}", part.message());
    }
    let mut corrupt: Vec<VetaError> = report
        .broken
        .iter()
        .chain(&report.corrupt)
        .map(DetachedPart::error)
        .collect();
    println!(
        "check: {} parts, {} blocks, {} corrupt",
        report.parts,
        report.blocks,
        corrupt.len()
    );
    match corrupt.pop() {
        Some(last) => {
            for err in &corrupt {
                eprintln!("veta: {err}");
            }
            Err(last)
        }
        None => Ok(()),
    }
}

/// Bytes of the files of a part directory.
fn directory_bytes(dir: &Path) -> Result<u64> {
    let mut total = 0;
    for entry in fs::read_dir(dir)? {
        total += entry?.metadata()?.len();
    }
    Ok(total)
}

/// `parts --db D --table T`: `name, rows, level, bytes` of each active part.
pub(crate) fn cmd_parts(args: &[String]) -> Result<()> {
    let parsed = Parsed::new(args, &["--db", "--table"], &[], &[])?;
    parsed.no_positional()?;
    let db = Database::open(parsed.required("--db")?);
    let table = db.open_table(parsed.required("--table")?)?;
    let snapshot = Snapshot::take(&table, Some(&[]))?;
    for detached in snapshot.detached() {
        eprintln!("{}", detached.message());
    }
    let stdout = io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    for part in snapshot.parts() {
        let name = part.name();
        writeln!(
            out,
            "{name}\t{}\t{}\t{}",
            part.rows(),
            name.level,
            directory_bytes(part.dir())?
        )?;
    }
    out.flush()?;
    Ok(())
}

/// Booleans of a `.raw` file, as bits LSB-first like `Plain`.
#[derive(Default)]
struct BitWriter {
    current: u8,
    filled: u32,
}

impl BitWriter {
    fn push(&mut self, out: &mut impl Write, bit: bool) -> Result<()> {
        self.current |= (bit as u8) << self.filled;
        self.filled += 1;
        if self.filled == 8 {
            out.write_all(&[self.current])?;
            self.current = 0;
            self.filled = 0;
        }
        Ok(())
    }

    fn finish(self, out: &mut impl Write) -> Result<()> {
        if self.filled > 0 {
            out.write_all(&[self.current])?;
        }
        Ok(())
    }
}

fn write_le<const N: usize, T: Copy>(
    out: &mut impl Write,
    values: &[T],
    v: &Vector,
    f: impl Fn(T) -> [u8; N],
) -> Result<()> {
    for (i, &value) in values.iter().enumerate() {
        if v.is_valid(i) {
            out.write_all(&f(value))?;
        }
    }
    Ok(())
}

/// Appends the non-NULL values of `v` in `Plain`: fixed-width values in little-endian and
/// strings as `u32` LE length + bytes (contract §13).
fn write_raw(out: &mut impl Write, bits: &mut BitWriter, v: &Vector) -> Result<()> {
    match &v.data {
        VectorData::Boolean(x) => {
            for (i, &value) in x.iter().enumerate() {
                if v.is_valid(i) {
                    bits.push(out, value)?;
                }
            }
            Ok(())
        }
        VectorData::Int16(x) => write_le(out, x, v, |a: i16| a.to_le_bytes()),
        VectorData::Int32(x) | VectorData::Date(x) => write_le(out, x, v, |a: i32| a.to_le_bytes()),
        VectorData::Int64(x) | VectorData::Timestamp(x) => {
            write_le(out, x, v, |a: i64| a.to_le_bytes())
        }
        VectorData::Int128(x) => write_le(out, x, v, |a: i128| a.to_le_bytes()),
        VectorData::Float64(x) => write_le(out, x, v, |a: f64| a.to_le_bytes()),
        VectorData::Decimal64 { values, .. } => write_le(out, values, v, |a: i64| a.to_le_bytes()),
        VectorData::Decimal128 { values, .. } => {
            write_le(out, values, v, |a: i128| a.to_le_bytes())
        }
        VectorData::Varchar { offsets, bytes } => {
            for i in 0..v.len() {
                if v.is_valid(i) {
                    let value = &bytes[offsets[i] as usize..offsets[i + 1] as usize];
                    out.write_all(&(value.len() as u32).to_le_bytes())?;
                    out.write_all(value)?;
                }
            }
            Ok(())
        }
    }
}

/// `dump-columns --db D --table T --out DIR`: one `<column>.raw` per column.
pub(crate) fn cmd_dump_columns(args: &[String]) -> Result<()> {
    let parsed = Parsed::new(args, &["--db", "--table", "--out"], &[], &[])?;
    parsed.no_positional()?;
    let db = Database::open(parsed.required("--db")?);
    let table = db.open_table(parsed.required("--table")?)?;
    let dir = Path::new(parsed.required("--out")?);
    fs::create_dir_all(dir)?;
    let snapshot = Snapshot::take(&table, None)?;
    for detached in snapshot.detached() {
        eprintln!("{}", detached.message());
    }
    for (column, field) in table.schema().fields.iter().enumerate() {
        let file_name = format!("{}.raw", field.name.replace('/', "_"));
        let mut out = BufWriter::with_capacity(1 << 20, File::create(dir.join(file_name))?);
        let mut bits = BitWriter::default();
        for part in snapshot.parts() {
            for granule in 0..part.granules() {
                let block = read_or_detach(&table, part, column, granule)?;
                write_raw(&mut out, &mut bits, &block)?;
            }
        }
        bits.finish(&mut out)?;
        out.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use veta_core::{DataType, Value};

    fn raw(v: &Vector) -> Vec<u8> {
        let mut out = Vec::new();
        let mut bits = BitWriter::default();
        write_raw(&mut out, &mut bits, v).unwrap();
        bits.finish(&mut out).unwrap();
        out
    }

    #[test]
    fn raw_values_skip_nulls() {
        let ints = Vector::from_values(
            DataType::Int32,
            &[Value::Int32(1), Value::Null, Value::Int32(2)],
        )
        .unwrap();
        assert_eq!(raw(&ints), [1, 0, 0, 0, 2, 0, 0, 0]);
        let strings =
            Vector::from_values(DataType::Varchar, &[Value::varchar("ab"), Value::Null]).unwrap();
        assert_eq!(raw(&strings), [2, 0, 0, 0, b'a', b'b']);
    }

    #[test]
    fn raw_booleans_are_bits() {
        let values: Vec<Value> = [true, false, true, true, false, false, false, false, true]
            .iter()
            .map(|&b| Value::Boolean(b))
            .collect();
        let bools = Vector::from_values(DataType::Boolean, &values).unwrap();
        assert_eq!(raw(&bools), [0b0000_1101, 0b0000_0001]);
    }
}
