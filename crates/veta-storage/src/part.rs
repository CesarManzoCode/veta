//! Parts (contract §5.2): writing, opening with the integrity checks of §6.4 and reading
//! verified granules.
//!
//! A part is a directory `all_<min>_<max>_<level>` with its rows sorted by the primary
//! key, cut into granules of 8192 rows (the last one may be shorter):
//!
//! - `part.txt`: `format=1`, `rows`, `granules`, `index_granularity=8192`, `min_block`,
//!   `max_block`, `level`, then `column=<N>\t<name>\t<SQL type>\t<nullable 0/1>` per column.
//! - `primary.idx`: one block per key column (in key order) with `granules + 1` values, the
//!   first row of each granule and the last row of the part.
//! - `c<N>.bin`: one block per granule; `c<N>.mrk`: `granules + 1` `u64` LE offsets into it,
//!   the last one its length.
//! - `stats.bin`: per column the record of [`ColumnStats::encode`], then the 4096 HLL
//!   registers of each key prefix (lengths `1..=k`, see [`crate::colstats`]), then its own
//!   `u32` CRC32C.
//! - `sample.bin`: one block per column with the rows at `floor(i · rows / s)`,
//!   `s = min(1024, rows)`.
//! - `checksums.txt`: `<file>\t<bytes>\t<crc32c hex>` per other file and a last line
//!   `self\t<crc32c of the previous lines>`.
//!
//! [`PartWriter`] writes all of them, whole rows or one column at a time, and fsyncs every
//! file and the directory; publishing the directory (the `rename` of §6.1) is the caller's
//! job. [`PartReader::open`] checks
//! `checksums.txt`, the presence and size of every file, the full CRC32C of every file but
//! the `c<N>.bin`, and the consistency of `part.txt`, the marks, the index, the stats and the
//! sample with the table schema. [`PartReader::read_granule`] checks each block against its
//! marks (§5.3). Every integrity failure is [`VetaError::Corrupt`] naming the part.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::mem;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use veta_core::crc::{crc32c, crc32c_append};
use veta_core::hll::{HLL_REGISTERS, Hll};
use veta_core::stats::{ColumnStats, SAMPLE_ROWS, sample_positions};
use veta_core::{Chunk, DataType, Result, Schema, Vector, VetaError};

use crate::block::{read_block, read_block_exact, write_block};
use crate::colstats::{row_hashes, update_column_stats, update_prefix_hlls};
use crate::fsutil::{io_err, subdirectories, sync_dir, write_synced};

/// Rows per granule (contract §5.2).
pub const GRANULE_ROWS: usize = 8192;
/// Value of `format=` in `part.txt`.
pub const FORMAT_VERSION: u64 = 1;

pub const PART_TXT: &str = "part.txt";
pub const PRIMARY_IDX: &str = "primary.idx";
pub const STATS_BIN: &str = "stats.bin";
pub const SAMPLE_BIN: &str = "sample.bin";
pub const CHECKSUMS_TXT: &str = "checksums.txt";
/// Lock file of a part under construction; never part of a published part's checksums.
pub const OWNER_LOCK: &str = "owner.lock";

/// `c<N>.bin`.
pub fn bin_file(column: usize) -> String {
    format!("c{column}.bin")
}

/// `c<N>.mrk`.
pub fn mrk_file(column: usize) -> String {
    format!("c{column}.mrk")
}

/// Granules of a part of `rows` rows.
pub fn granule_count(rows: u64) -> usize {
    rows.div_ceil(GRANULE_ROWS as u64) as usize
}

/// Name of a part: `all_<min_block>_<max_block>_<level>` (contract §5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PartName {
    pub min_block: u64,
    pub max_block: u64,
    pub level: u32,
}

impl PartName {
    pub fn new(min_block: u64, max_block: u64, level: u32) -> PartName {
        PartName {
            min_block,
            max_block,
            level,
        }
    }

    /// Parses a canonical part name (decimal digits without leading zeros,
    /// `min_block ≤ max_block`); anything else is not a part.
    pub fn parse(name: &str) -> Option<PartName> {
        let rest = name.strip_prefix("all_")?;
        let mut fields = rest.split('_');
        let mut number = || -> Option<u64> {
            let text = fields.next()?;
            if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            text.parse().ok()
        };
        let min_block = number()?;
        let max_block = number()?;
        let level = u32::try_from(number()?).ok()?;
        if fields.next().is_some() || min_block > max_block {
            return None;
        }
        let part = PartName::new(min_block, max_block, level);
        (part.to_string() == name).then_some(part)
    }

    /// `self` covers `other` if its block range contains `other`'s and they differ
    /// (contract §5.1).
    pub fn covers(&self, other: &PartName) -> bool {
        self != other && self.min_block <= other.min_block && other.max_block <= self.max_block
    }
}

impl fmt::Display for PartName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "all_{}_{}_{}",
            self.min_block, self.max_block, self.level
        )
    }
}

/// The parts not covered by another part of `parts`, sorted by block range (contract
/// §5.1). Two different parts with the same range cover each other, so neither is active;
/// the protocol of §6 never produces that.
pub fn active_parts(parts: &[PartName]) -> Vec<PartName> {
    let mut active: Vec<PartName> = parts
        .iter()
        .filter(|p| !parts.iter().any(|q| q.covers(p)))
        .copied()
        .collect();
    active.sort();
    active.dedup();
    active
}

/// The parts of `parts` covered by another one.
pub fn covered_parts(parts: &[PartName]) -> Vec<PartName> {
    let mut covered: Vec<PartName> = parts
        .iter()
        .filter(|p| parts.iter().any(|q| q.covers(p)))
        .copied()
        .collect();
    covered.sort();
    covered.dedup();
    covered
}

/// The part directories present in a table directory (active or covered), sorted.
/// `tmp_*`, `delete_tmp_*`, `detached` and anything else are ignored.
pub fn list_parts(table_dir: &Path) -> Result<Vec<PartName>> {
    let mut parts: Vec<PartName> = subdirectories(table_dir)?
        .iter()
        .filter_map(|name| PartName::parse(name))
        .collect();
    parts.sort();
    Ok(parts)
}

/// Statistics of a part as stored in `stats.bin`.
#[derive(Debug, Clone, PartialEq)]
pub struct PartStats {
    pub columns: Vec<ColumnStats>,
    /// `key_prefix_hll[j]`: HLL of the key prefix of length `j + 1`.
    pub key_prefix_hll: Vec<Hll>,
}

/// What [`PartWriter::finish`] wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PartMeta {
    pub name: PartName,
    pub rows: u64,
    pub granules: usize,
}

struct ColumnFile {
    path: PathBuf,
    bin: BufWriter<File>,
    /// Bytes written so far, which is also the next mark.
    offset: u64,
    crc: u32,
    marks: Vec<u64>,
    stats: ColumnStats,
    /// Rows accepted so far.
    received: u64,
    /// Rows already in blocks.
    written: u64,
    /// Rows waiting to fill a granule.
    pending: Vector,
    sample: Vector,
    sample_next: usize,
    /// Position of this column in the primary key.
    key_position: Option<usize>,
}

/// Writes one part into an existing (temporary) directory from rows that are already
/// sorted by the primary key.
///
/// Rows arrive whole ([`PartWriter::write`]) or column by column, as the vertical merge
/// sends them ([`PartWriter::write_column`] per column and [`PartWriter::add_key_rows`]
/// for the key prefix HLLs); both produce the same files. The total row count is given up
/// front so the systematic sample can be taken while the rows stream through;
/// [`PartWriter::finish`] fails if a different number arrived.
pub struct PartWriter {
    dir: PathBuf,
    name: PartName,
    schema: Schema,
    primary_key: Vec<usize>,
    rows: u64,
    key_rows: u64,
    files: Vec<ColumnFile>,
    prefix_hll: Vec<Hll>,
    index: Vec<Vector>,
    last_key: Vec<Vector>,
    sample_positions: Vec<u64>,
    buf: Vec<u8>,
}

impl PartWriter {
    /// Starts part `name` of `rows` rows (at least one) in `dir`, which must exist.
    pub fn create(
        dir: &Path,
        name: PartName,
        schema: &Schema,
        primary_key: &[usize],
        rows: u64,
    ) -> Result<PartWriter> {
        if rows == 0 {
            return Err(VetaError::internal("a part needs at least one row"));
        }
        if schema.is_empty() {
            return Err(VetaError::internal("a part needs at least one column"));
        }
        if let Some(&bad) = primary_key.iter().find(|&&c| c >= schema.len()) {
            return Err(VetaError::internal(format!(
                "primary key column {bad} out of range"
            )));
        }
        let types = schema.data_types();
        let mut files = Vec::with_capacity(schema.len());
        for column in 0..schema.len() {
            let path = dir.join(bin_file(column));
            let file = File::create(&path).map_err(|e| io_err(&path, e))?;
            files.push(ColumnFile {
                path,
                bin: BufWriter::with_capacity(1 << 16, file),
                offset: 0,
                crc: 0,
                marks: vec![0],
                stats: ColumnStats::new(),
                received: 0,
                written: 0,
                pending: Vector::with_capacity(types[column], 0),
                sample: Vector::with_capacity(types[column], 0),
                sample_next: 0,
                key_position: primary_key.iter().position(|&c| c == column),
            });
        }
        let key_vector = |&c: &usize| Vector::with_capacity(types[c], 0);
        Ok(PartWriter {
            dir: dir.to_path_buf(),
            name,
            schema: schema.clone(),
            primary_key: primary_key.to_vec(),
            rows,
            key_rows: 0,
            files,
            prefix_hll: vec![Hll::new(); primary_key.len()],
            index: primary_key.iter().map(key_vector).collect(),
            last_key: primary_key.iter().map(key_vector).collect(),
            sample_positions: sample_positions(rows),
            buf: Vec::new(),
        })
    }

    pub fn name(&self) -> PartName {
        self.name
    }

    /// Appends rows, which continue the key order of the previous ones.
    pub fn write(&mut self, chunk: &Chunk) -> Result<()> {
        self.check_chunk(chunk)?;
        if chunk.len == 0 {
            return Ok(());
        }
        let key: Vec<&Vector> = self.primary_key.iter().map(|&c| chunk.column(c)).collect();
        self.add_key_rows(&key)?;
        for (column, vector) in chunk.columns.iter().enumerate() {
            self.append_column(column, vector)?;
        }
        Ok(())
    }

    /// Appends rows of one column, which continue that column's previous ones. Every
    /// column must end up with the part's rows and in the same row order.
    pub fn write_column(&mut self, column: usize, vector: &Vector) -> Result<()> {
        self.check_column(column, vector)?;
        self.append_column(column, vector)
    }

    /// Adds rows of the key columns (in key order) to the key prefix HLLs, which depend on
    /// the columns of a row together. Their order among themselves does not matter, so the
    /// vertical merge passes them as it loads them from each source.
    pub fn add_key_rows(&mut self, key: &[&Vector]) -> Result<()> {
        if key.len() != self.primary_key.len() {
            return Err(VetaError::internal(format!(
                "{} key columns for a key of {}",
                key.len(),
                self.primary_key.len()
            )));
        }
        let Some(first) = key.first() else {
            return Ok(());
        };
        for (vector, &c) in key.iter().zip(&self.primary_key) {
            let data_type = self.schema.field(c).data_type;
            if vector.len() != first.len() || vector.data_type() != data_type {
                return Err(VetaError::internal(format!(
                    "key column {} got {} {} rows, expected {} {data_type} rows",
                    self.schema.field(c).name,
                    vector.len(),
                    vector.data_type(),
                    first.len()
                )));
            }
        }
        if self.key_rows + first.len() as u64 > self.rows {
            return Err(VetaError::internal(format!(
                "part {} was declared with {} rows and got more key rows",
                self.name, self.rows
            )));
        }
        self.key_rows += first.len() as u64;
        update_prefix_hlls(&mut self.prefix_hll, key);
        Ok(())
    }

    fn check_chunk(&self, chunk: &Chunk) -> Result<()> {
        if chunk.num_columns() != self.schema.len() {
            return Err(VetaError::internal(format!(
                "chunk has {} columns, part {} has {}",
                chunk.num_columns(),
                self.name,
                self.schema.len()
            )));
        }
        for (column, vector) in chunk.columns.iter().enumerate() {
            self.check_column(column, vector)?;
        }
        Ok(())
    }

    fn check_column(&self, column: usize, vector: &Vector) -> Result<()> {
        let (Some(field), Some(file)) = (self.schema.fields.get(column), self.files.get(column))
        else {
            return Err(VetaError::internal(format!(
                "column {column} out of range in part {}",
                self.name
            )));
        };
        if vector.data_type() != field.data_type {
            return Err(VetaError::internal(format!(
                "column {} is {}, got a {} vector",
                field.name,
                field.data_type,
                vector.data_type()
            )));
        }
        if !field.nullable && vector.null_count() > 0 {
            return Err(VetaError::internal(format!(
                "NULL in NOT NULL column {}",
                field.name
            )));
        }
        if file.received + vector.len() as u64 > self.rows {
            return Err(VetaError::internal(format!(
                "part {} was declared with {} rows and got more",
                self.name, self.rows
            )));
        }
        Ok(())
    }

    /// Updates the statistics of `column` and cuts its rows into granules.
    fn append_column(&mut self, column: usize, vector: &Vector) -> Result<()> {
        let len = vector.len();
        let file = &mut self.files[column];
        update_column_stats(&mut file.stats, vector, &row_hashes(vector));
        file.received += len as u64;

        let mut offset = 0;
        while offset < len {
            let left = len - offset;
            let pending = self.files[column].pending.len();
            if pending == 0 && left >= GRANULE_ROWS {
                let slice = (len != GRANULE_ROWS).then(|| vector.slice(offset, GRANULE_ROWS));
                self.flush_granule(column, slice.as_ref().unwrap_or(vector))?;
                offset += GRANULE_ROWS;
            } else {
                let take = left.min(GRANULE_ROWS - pending);
                self.files[column]
                    .pending
                    .extend(&vector.slice(offset, take))?;
                offset += take;
                if self.files[column].pending.len() == GRANULE_ROWS {
                    self.flush_pending(column)?;
                }
            }
        }
        Ok(())
    }

    fn flush_pending(&mut self, column: usize) -> Result<()> {
        let data_type = self.schema.field(column).data_type;
        let empty = Vector::with_capacity(data_type, 0);
        let granule = mem::replace(&mut self.files[column].pending, empty);
        self.flush_granule(column, &granule)
    }

    /// Writes one granule of `column`: its block, its mark, its index entries if it is a
    /// key column and the sample rows that fall inside it.
    fn flush_granule(&mut self, column: usize, granule: &Vector) -> Result<()> {
        let len = granule.len();
        let file = &mut self.files[column];
        self.buf.clear();
        write_block(granule, &mut self.buf)?;
        file.bin
            .write_all(&self.buf)
            .map_err(|e| io_err(&file.path, e))?;
        file.crc = crc32c_append(file.crc, &self.buf);
        file.offset += self.buf.len() as u64;
        file.marks.push(file.offset);
        if let Some(j) = file.key_position {
            self.index[j].extend(&granule.slice(0, 1))?;
            self.last_key[j] = granule.slice(len - 1, 1);
        }
        let start = file.written;
        let end = start + len as u64;
        let mut picks = Vec::new();
        while let Some(&pos) = self.sample_positions.get(file.sample_next) {
            if pos >= end {
                break;
            }
            picks.push((pos - start) as u32);
            file.sample_next += 1;
        }
        if !picks.is_empty() {
            file.sample.extend(&granule.gather(&picks))?;
        }
        file.written = end;
        Ok(())
    }

    /// Writes the remaining files, fsyncs each of them and the directory.
    pub fn finish(mut self) -> Result<PartMeta> {
        for column in 0..self.files.len() {
            if !self.files[column].pending.is_empty() {
                self.flush_pending(column)?;
            }
            if self.files[column].written != self.rows {
                return Err(VetaError::internal(format!(
                    "part {} was declared with {} rows and got {}",
                    self.name, self.rows, self.files[column].written
                )));
            }
        }
        if !self.primary_key.is_empty() && self.key_rows != self.rows {
            return Err(VetaError::internal(format!(
                "part {} was declared with {} rows and got {} key rows",
                self.name, self.rows, self.key_rows
            )));
        }
        let granules = granule_count(self.rows);
        let mut written: Vec<(String, u64, u32)> = Vec::new();

        self.write_file(PART_TXT, self.part_txt(granules).as_bytes(), &mut written)?;

        let mut idx = Vec::new();
        for (index, last) in self.index.iter().zip(&self.last_key) {
            let mut values = index.clone();
            values.extend(last)?;
            write_block(&values, &mut idx)?;
        }
        self.write_file(PRIMARY_IDX, &idx, &mut written)?;

        let mut stats = Vec::new();
        let mut sample = Vec::new();
        for (column, mut file) in mem::take(&mut self.files).into_iter().enumerate() {
            file.bin.flush().map_err(|e| io_err(&file.path, e))?;
            file.bin
                .get_ref()
                .sync_all()
                .map_err(|e| io_err(&file.path, e))?;
            written.push((bin_file(column), file.offset, file.crc));
            let marks: Vec<u8> = file.marks.iter().flat_map(|m| m.to_le_bytes()).collect();
            self.write_file(&mrk_file(column), &marks, &mut written)?;
            file.stats
                .encode(self.schema.field(column).data_type, &mut stats)?;
            write_block(&file.sample, &mut sample)?;
        }

        for hll in &self.prefix_hll {
            stats.extend_from_slice(hll.registers());
        }
        let crc = crc32c(&stats);
        stats.extend_from_slice(&crc.to_le_bytes());
        self.write_file(STATS_BIN, &stats, &mut written)?;

        self.write_file(SAMPLE_BIN, &sample, &mut written)?;

        let mut checksums = String::new();
        for (name, len, crc) in &written {
            checksums.push_str(&format!("{name}\t{len}\t{crc:08x}\n"));
        }
        let self_crc = crc32c(checksums.as_bytes());
        checksums.push_str(&format!("self\t{self_crc:08x}\n"));
        write_synced(&self.dir.join(CHECKSUMS_TXT), checksums.as_bytes())?;
        sync_dir(&self.dir)?;
        Ok(PartMeta {
            name: self.name,
            rows: self.rows,
            granules,
        })
    }

    fn write_file(
        &self,
        name: &str,
        bytes: &[u8],
        written: &mut Vec<(String, u64, u32)>,
    ) -> Result<()> {
        write_synced(&self.dir.join(name), bytes)?;
        written.push((name.to_string(), bytes.len() as u64, crc32c(bytes)));
        Ok(())
    }

    fn part_txt(&self, granules: usize) -> String {
        let mut text = format!(
            "format={FORMAT_VERSION}\nrows={}\ngranules={granules}\nindex_granularity={GRANULE_ROWS}\n\
             min_block={}\nmax_block={}\nlevel={}\n",
            self.rows, self.name.min_block, self.name.max_block, self.name.level
        );
        for (i, field) in self.schema.fields.iter().enumerate() {
            text.push_str(&format!(
                "column={i}\t{}\t{}\t{}\n",
                field.name,
                field.data_type.sql_name(),
                field.nullable as u8
            ));
        }
        text
    }
}

/// An opened, verified part. The `c<N>.bin` of the requested columns stay open, so the
/// data survives a later deletion of the directory (contract §6.3).
#[derive(Debug)]
pub struct PartReader {
    dir: PathBuf,
    name: PartName,
    label: String,
    schema: Schema,
    primary_key: Vec<usize>,
    rows: u64,
    granules: usize,
    marks: Vec<Vec<u64>>,
    files: Vec<Option<File>>,
    index: Vec<Vector>,
    stats: PartStats,
    sample: Chunk,
}

/// `Corrupt` for a missing file if the part directory still exists; otherwise the I/O
/// error, so a caller can tell a vanished part (`ENOENT` on the directory) from a broken one.
fn open_error(dir: &Path, path: &Path, err: io::Error, part: &str) -> VetaError {
    if err.kind() == io::ErrorKind::NotFound && dir.is_dir() {
        let file = path.file_name().map(|f| f.to_string_lossy().into_owned());
        VetaError::corrupt(part, format!("missing file {}", file.unwrap_or_default()))
    } else {
        io_err(path, err)
    }
}

/// The message of an error without its kind prefix.
fn reason_of(err: &VetaError) -> String {
    match err {
        VetaError::User(m) | VetaError::Io(m) | VetaError::Internal(m) => m.clone(),
        VetaError::Corrupt { reason, .. } => reason.clone(),
        other => other.to_string(),
    }
}

struct ChecksumEntry {
    file: String,
    bytes: u64,
    crc: u32,
}

fn parse_hex32(text: &str) -> std::result::Result<u32, String> {
    let ok = text.len() == 8 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !ok {
        return Err(format!("invalid crc32c {text:?}"));
    }
    u32::from_str_radix(text, 16).map_err(|_| format!("invalid crc32c {text:?}"))
}

/// Parses `checksums.txt` and verifies its `self` line.
fn parse_checksums(bytes: &[u8]) -> std::result::Result<Vec<ChecksumEntry>, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "checksums.txt is not UTF-8")?;
    let body = text
        .strip_suffix('\n')
        .ok_or("checksums.txt does not end with a newline")?;
    let (lines, self_line) = match body.rfind('\n') {
        Some(at) => (&text[..at + 1], &body[at + 1..]),
        None => ("", body),
    };
    let self_hex = self_line
        .strip_prefix("self\t")
        .ok_or("checksums.txt does not end with its self line")?;
    let expected = parse_hex32(self_hex)?;
    let actual = crc32c(lines.as_bytes());
    if actual != expected {
        return Err(format!(
            "checksums.txt crc32c {actual:08x} does not match its self line {expected:08x}"
        ));
    }
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for line in lines.strip_suffix('\n').unwrap_or("").split('\n') {
        if lines.is_empty() {
            break;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let [file, size, crc] = fields.as_slice() else {
            return Err(format!("invalid checksums.txt line {line:?}"));
        };
        let size_ok = !size.is_empty() && size.bytes().all(|b| b.is_ascii_digit());
        let bytes = size
            .parse::<u64>()
            .ok()
            .filter(|_| size_ok)
            .ok_or_else(|| format!("invalid size in checksums.txt line {line:?}"))?;
        if !seen.insert(file.to_string()) {
            return Err(format!("checksums.txt lists {file} twice"));
        }
        entries.push(ChecksumEntry {
            file: file.to_string(),
            bytes,
            crc: parse_hex32(crc)?,
        });
    }
    Ok(entries)
}

/// The fields of `part.txt` a reader needs.
struct PartTxt {
    rows: u64,
    granules: usize,
    name: PartName,
}

const PART_TXT_KEYS: [&str; 7] = [
    "format",
    "rows",
    "granules",
    "index_granularity",
    "min_block",
    "max_block",
    "level",
];

/// Parses `part.txt` and checks it against the table schema.
fn parse_part_txt(bytes: &[u8], schema: &Schema) -> std::result::Result<PartTxt, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "part.txt is not UTF-8")?;
    let body = text
        .strip_suffix('\n')
        .ok_or("part.txt does not end with a newline")?;
    let mut fields: HashMap<&str, &str> = HashMap::new();
    let mut columns = Vec::new();
    for line in body.split('\n') {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("part.txt line without '=': {line:?}"))?;
        if key == "column" {
            columns.push(value);
        } else if !PART_TXT_KEYS.contains(&key) {
            return Err(format!("unknown part.txt key {key:?}"));
        } else if fields.insert(key, value).is_some() {
            return Err(format!("part.txt repeats {key}"));
        }
    }
    let number = |key: &str| -> std::result::Result<u64, String> {
        let text = fields
            .get(key)
            .ok_or_else(|| format!("part.txt lacks {key}"))?;
        let digits = !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
        text.parse::<u64>()
            .ok()
            .filter(|_| digits)
            .ok_or_else(|| format!("invalid part.txt {key}={text}"))
    };
    if number("format")? != FORMAT_VERSION {
        return Err(format!("unsupported part format {}", number("format")?));
    }
    if number("index_granularity")? != GRANULE_ROWS as u64 {
        return Err(format!(
            "index_granularity {} is not {GRANULE_ROWS}",
            number("index_granularity")?
        ));
    }
    let rows = number("rows")?;
    let granules = number("granules")?;
    if rows == 0 || granules != granule_count(rows) as u64 {
        return Err(format!("{rows} rows do not make {granules} granules"));
    }
    let level = u32::try_from(number("level")?).map_err(|_| "level out of range")?;
    let name = PartName::new(number("min_block")?, number("max_block")?, level);
    if columns.len() != schema.len() {
        return Err(format!(
            "part has {} columns, the table {}",
            columns.len(),
            schema.len()
        ));
    }
    for (i, (line, field)) in columns.iter().zip(&schema.fields).enumerate() {
        let parts: Vec<&str> = line.split('\t').collect();
        let matches = match parts.as_slice() {
            [n, name, ty, nullable] => {
                *n == i.to_string()
                    && *name == field.name
                    && DataType::parse(ty).ok() == Some(field.data_type)
                    && *nullable == if field.nullable { "1" } else { "0" }
            }
            _ => false,
        };
        if !matches {
            return Err(format!(
                "part column {line:?} does not match table column {i} {} {}{}",
                field.name,
                field.data_type,
                if field.nullable { "" } else { " NOT NULL" }
            ));
        }
    }
    Ok(PartTxt {
        rows,
        granules: granules as usize,
        name,
    })
}

/// Parses and checks the marks of a column: `granules + 1` offsets from 0, each block at
/// least a header long, the last one the length of `c<N>.bin`.
fn parse_marks(
    bytes: &[u8],
    granules: usize,
    bin_len: u64,
) -> std::result::Result<Vec<u64>, String> {
    if bytes.len() != (granules + 1) * 8 {
        return Err(format!(
            "{} bytes of marks for {granules} granules",
            bytes.len()
        ));
    }
    let marks: Vec<u64> = bytes
        .chunks_exact(8)
        .map(|b| u64::from_le_bytes(b.try_into().expect("8 bytes")))
        .collect();
    if marks[0] != 0 {
        return Err(format!("first mark is {}", marks[0]));
    }
    let header = crate::block::HEADER_LEN as u64;
    if let Some(w) = marks
        .windows(2)
        .find(|w| w[1] < w[0].saturating_add(header))
    {
        return Err(format!("marks {} and {} do not hold a block", w[0], w[1]));
    }
    if marks[granules] != bin_len {
        return Err(format!(
            "last mark {} is not the data length {bin_len}",
            marks[granules]
        ));
    }
    Ok(marks)
}

impl PartReader {
    /// Opens and verifies the part in `dir`, keeping every column open.
    pub fn open(dir: &Path, schema: &Schema, primary_key: &[usize]) -> Result<PartReader> {
        let all: Vec<usize> = (0..schema.len()).collect();
        PartReader::open_columns(dir, schema, primary_key, &all)
    }

    /// Opens and verifies the part in `dir` (contract §6.4), keeping open only the
    /// `c<N>.bin` of `columns`. Every file of the part is checked whatever `columns` is.
    pub fn open_columns(
        dir: &Path,
        schema: &Schema,
        primary_key: &[usize],
        columns: &[usize],
    ) -> Result<PartReader> {
        let label = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let name = PartName::parse(&label).ok_or_else(|| {
            VetaError::internal(format!("{} is not a part directory", dir.display()))
        })?;
        if let Some(&bad) = primary_key
            .iter()
            .chain(columns)
            .find(|&&c| c >= schema.len())
        {
            return Err(VetaError::internal(format!("column {bad} out of range")));
        }
        let corrupt = |reason: String| VetaError::corrupt(&label, reason);

        let checksums_path = dir.join(CHECKSUMS_TXT);
        let checksums =
            fs::read(&checksums_path).map_err(|e| open_error(dir, &checksums_path, e, &label))?;
        let entries = parse_checksums(&checksums).map_err(corrupt)?;

        let mut expected: Vec<String> = [PART_TXT, PRIMARY_IDX, STATS_BIN, SAMPLE_BIN]
            .iter()
            .map(|s| s.to_string())
            .collect();
        for column in 0..schema.len() {
            expected.push(bin_file(column));
            expected.push(mrk_file(column));
        }
        let listed: HashSet<&str> = entries.iter().map(|e| e.file.as_str()).collect();
        if let Some(missing) = expected.iter().find(|f| !listed.contains(f.as_str())) {
            return Err(corrupt(format!("checksums.txt does not list {missing}")));
        }
        if entries.len() != expected.len() {
            let known: HashSet<&str> = expected.iter().map(String::as_str).collect();
            let extra = entries
                .iter()
                .find(|e| !known.contains(e.file.as_str()))
                .map_or("?", |e| e.file.as_str());
            return Err(corrupt(format!("checksums.txt lists unknown file {extra}")));
        }

        let wanted: HashSet<usize> = columns.iter().copied().collect();
        let mut files: Vec<Option<File>> = (0..schema.len()).map(|_| None).collect();
        let mut bin_len = vec![0u64; schema.len()];
        let mut contents: HashMap<&str, Vec<u8>> = HashMap::new();
        for entry in &entries {
            let path = dir.join(&entry.file);
            let size_error = |actual: u64| {
                corrupt(format!(
                    "{} has {actual} bytes, checksums.txt says {}",
                    entry.file, entry.bytes
                ))
            };
            let column = entry
                .file
                .strip_prefix('c')
                .and_then(|rest| rest.strip_suffix(".bin"))
                .and_then(|n| n.parse::<usize>().ok());
            if let Some(column) = column {
                let len = if wanted.contains(&column) {
                    let file = File::open(&path).map_err(|e| open_error(dir, &path, e, &label))?;
                    let len = file.metadata().map_err(|e| io_err(&path, e))?.len();
                    files[column] = Some(file);
                    len
                } else {
                    fs::metadata(&path)
                        .map_err(|e| open_error(dir, &path, e, &label))?
                        .len()
                };
                if len != entry.bytes {
                    return Err(size_error(len));
                }
                bin_len[column] = len;
            } else {
                let bytes = fs::read(&path).map_err(|e| open_error(dir, &path, e, &label))?;
                if bytes.len() as u64 != entry.bytes {
                    return Err(size_error(bytes.len() as u64));
                }
                let crc = crc32c(&bytes);
                if crc != entry.crc {
                    return Err(corrupt(format!(
                        "{} has crc32c {crc:08x}, checksums.txt says {:08x}",
                        entry.file, entry.crc
                    )));
                }
                contents.insert(entry.file.as_str(), bytes);
            }
        }
        let content = |file: &str| contents.get(file).map(Vec::as_slice).unwrap_or(&[]);

        let txt = parse_part_txt(content(PART_TXT), schema).map_err(corrupt)?;
        if txt.name != name {
            return Err(corrupt(format!(
                "part.txt describes {}, the directory is {name}",
                txt.name
            )));
        }
        let (rows, granules) = (txt.rows, txt.granules);

        let mut marks = Vec::with_capacity(schema.len());
        for (column, &len) in bin_len.iter().enumerate() {
            let file = mrk_file(column);
            let parsed = parse_marks(content(file.as_str()), granules, len)
                .map_err(|reason| corrupt(format!("{file}: {reason}")))?;
            marks.push(parsed);
        }

        let idx = content(PRIMARY_IDX);
        let mut pos = 0;
        let mut index = Vec::with_capacity(primary_key.len());
        for &c in primary_key {
            let field = schema.field(c);
            let (vector, len) = read_block(
                &idx[pos..],
                field.data_type,
                field.nullable,
                granules + 1,
                &label,
            )?;
            index.push(vector);
            pos += len;
        }
        if pos != idx.len() {
            return Err(corrupt(format!(
                "primary.idx has {} bytes after its blocks",
                idx.len() - pos
            )));
        }

        let stats = parse_stats(content(STATS_BIN), schema, primary_key.len(), rows)
            .map_err(|reason| corrupt(format!("stats.bin: {reason}")))?;

        let sample_bytes = content(SAMPLE_BIN);
        let sample_rows = rows.min(SAMPLE_ROWS) as usize;
        let mut pos = 0;
        let mut sample_columns = Vec::with_capacity(schema.len());
        for field in &schema.fields {
            let (vector, len) = read_block(
                &sample_bytes[pos..],
                field.data_type,
                field.nullable,
                sample_rows,
                &label,
            )?;
            sample_columns.push(vector);
            pos += len;
        }
        if pos != sample_bytes.len() {
            return Err(corrupt(format!(
                "sample.bin has {} bytes after its blocks",
                sample_bytes.len() - pos
            )));
        }
        let sample = Chunk::with_len(sample_columns, sample_rows)
            .map_err(|e| corrupt(format!("sample.bin: {}", reason_of(&e))))?;

        Ok(PartReader {
            dir: dir.to_path_buf(),
            name,
            label,
            schema: schema.clone(),
            primary_key: primary_key.to_vec(),
            rows,
            granules,
            marks,
            files,
            index,
            stats,
            sample,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn name(&self) -> PartName {
        self.name
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    pub fn primary_key(&self) -> &[usize] {
        &self.primary_key
    }

    pub fn rows(&self) -> u64 {
        self.rows
    }

    pub fn granules(&self) -> usize {
        self.granules
    }

    /// Rows of granule `granule` (8192 but for a shorter last one).
    pub fn granule_rows(&self, granule: usize) -> usize {
        let start = granule as u64 * GRANULE_ROWS as u64;
        self.rows.saturating_sub(start).min(GRANULE_ROWS as u64) as usize
    }

    /// Per key column (in key order), its value at the first row of each granule followed
    /// by its value at the last row of the part (`granules + 1` values).
    pub fn index(&self) -> &[Vector] {
        &self.index
    }

    pub fn stats(&self) -> &PartStats {
        &self.stats
    }

    pub fn sample(&self) -> &Chunk {
        &self.sample
    }

    /// The `granules + 1` marks of `column`.
    pub fn marks(&self, column: usize) -> &[u64] {
        &self.marks[column]
    }

    /// Whether `c<N>.bin` of `column` was opened.
    pub fn has_column(&self, column: usize) -> bool {
        self.files.get(column).is_some_and(Option::is_some)
    }

    /// Reads and verifies the block of `column` in `granule` (contract §5.3): it must
    /// start at its mark, end at the next one, hold the granule's rows and pass its CRC.
    pub fn read_granule(&self, column: usize, granule: usize) -> Result<Vector> {
        let field = self.schema.fields.get(column).ok_or_else(|| {
            VetaError::internal(format!("column {column} out of range in {}", self.label))
        })?;
        if granule >= self.granules {
            return Err(VetaError::internal(format!(
                "granule {granule} out of range in {} ({} granules)",
                self.label, self.granules
            )));
        }
        let file = self.files[column].as_ref().ok_or_else(|| {
            VetaError::internal(format!("column {column} of {} was not opened", self.label))
        })?;
        let marks = &self.marks[column];
        let (start, end) = (marks[granule], marks[granule + 1]);
        let mut buf = vec![0u8; (end - start) as usize];
        file.read_exact_at(&mut buf, start).map_err(|e| {
            if e.kind() == io::ErrorKind::UnexpectedEof {
                VetaError::corrupt(
                    &self.label,
                    format!("{} is shorter than its marks", bin_file(column)),
                )
            } else {
                io_err(&self.dir.join(bin_file(column)), e)
            }
        })?;
        read_block_exact(
            &buf,
            field.data_type,
            field.nullable,
            self.granule_rows(granule),
            &self.label,
        )
    }

    /// Every granule of `column`, concatenated.
    pub fn read_column(&self, column: usize) -> Result<Vector> {
        let data_type = self
            .schema
            .fields
            .get(column)
            .map(|f| f.data_type)
            .ok_or_else(|| VetaError::internal(format!("column {column} out of range")))?;
        let mut out = Vector::with_capacity(data_type, self.rows as usize);
        for granule in 0..self.granules {
            out.extend(&self.read_granule(column, granule)?)?;
        }
        Ok(out)
    }
}

/// Parses `stats.bin`: its trailing CRC32C, one record per column and `key_len` prefix
/// HLLs, with nothing left over and row counts equal to the part's.
fn parse_stats(
    bytes: &[u8],
    schema: &Schema,
    key_len: usize,
    rows: u64,
) -> std::result::Result<PartStats, String> {
    if bytes.len() < 4 {
        return Err("truncated".into());
    }
    let (body, tail) = bytes.split_at(bytes.len() - 4);
    let stored = u32::from_le_bytes(tail.try_into().expect("4 bytes"));
    let actual = crc32c(body);
    if stored != actual {
        return Err(format!(
            "crc32c {actual:08x} does not match its trailer {stored:08x}"
        ));
    }
    let mut pos = 0;
    let mut columns = Vec::with_capacity(schema.len());
    for field in &schema.fields {
        let stats = ColumnStats::decode(body, &mut pos, field.data_type)
            .map_err(|e| format!("column {}: {}", field.name, reason_of(&e)))?;
        if stats.rows != rows || stats.nulls > rows || (!field.nullable && stats.nulls > 0) {
            return Err(format!(
                "column {} has {} rows and {} nulls in a part of {rows} rows",
                field.name, stats.rows, stats.nulls
            ));
        }
        columns.push(stats);
    }
    let mut key_prefix_hll = Vec::with_capacity(key_len);
    for j in 0..key_len {
        let registers = body
            .get(pos..pos + HLL_REGISTERS)
            .ok_or_else(|| format!("truncated key prefix HLL {}", j + 1))?;
        key_prefix_hll.push(Hll::from_bytes(registers).map_err(|e| reason_of(&e))?);
        pos += HLL_REGISTERS;
    }
    if pos != body.len() {
        return Err(format!("{} bytes after the records", body.len() - pos));
    }
    Ok(PartStats {
        columns,
        key_prefix_hll,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::fsutil::testdir::TestDir;
    use veta_core::rand::Rng;
    use veta_core::{Field, Value};

    pub(crate) fn all_types_schema() -> Schema {
        let types = [
            DataType::Boolean,
            DataType::Int16,
            DataType::Int32,
            DataType::Int64,
            DataType::Float64,
            DataType::decimal(15, 2).unwrap(),
            DataType::decimal(30, 4).unwrap(),
            DataType::Varchar,
            DataType::Date,
            DataType::Timestamp,
        ];
        let mut fields = Vec::new();
        for (i, t) in types.iter().enumerate() {
            fields.push(Field::new(format!("n{i}"), *t, true));
            fields.push(Field::new(format!("v{i}"), *t, false));
        }
        Schema::new(fields)
    }

    fn random_value(rng: &mut Rng, data_type: DataType, row: usize) -> Value {
        match data_type {
            DataType::Boolean => Value::Boolean(rng.next_bool(0.5)),
            DataType::Int16 => Value::Int16(rng.range_i64(-300, 300) as i16),
            DataType::Int32 => Value::Int32(rng.range_i64(-50, 50) as i32),
            DataType::Int64 => Value::Int64(row as i64 * 3 - rng.range_i64(0, 2)),
            DataType::Int128 => Value::Int128(rng.next_u64() as i128),
            DataType::Float64 => Value::Float64(rng.next_f64() * 1e6 - 5e5),
            DataType::Decimal { precision, scale } => Value::Decimal {
                value: rng.range_i64(-1_000_000_000, 1_000_000_000) as i128,
                precision,
                scale,
            },
            DataType::Varchar => {
                let len = rng.below(12) as usize;
                Value::Varchar((0..len).map(|_| b'a' + rng.below(6) as u8).collect())
            }
            DataType::Date => Value::Date(rng.range_i64(-1000, 20_000) as i32),
            DataType::Timestamp => Value::Timestamp(rng.range_i64(0, 2_000_000_000)),
        }
    }

    /// Rows of `schema`; nullable columns get ~15 % NULLs (all NULL in a 1-row part
    /// half the time).
    pub(crate) fn random_chunk(schema: &Schema, rows: usize, seed: u64) -> Chunk {
        let mut rng = Rng::new(seed);
        let mut columns = Vec::new();
        for field in &schema.fields {
            let values: Vec<Value> = (0..rows)
                .map(|row| {
                    if field.nullable && rng.next_bool(0.15) {
                        Value::Null
                    } else {
                        random_value(&mut rng, field.data_type, row)
                    }
                })
                .collect();
            columns.push(Vector::from_values(field.data_type, &values).unwrap());
        }
        Chunk::with_len(columns, rows).unwrap()
    }

    /// Writes `chunk` (already sorted) as part `name` under `root`, in pieces of `step`.
    pub(crate) fn write_part(
        root: &Path,
        name: PartName,
        schema: &Schema,
        key: &[usize],
        chunk: &Chunk,
        step: usize,
    ) -> PathBuf {
        let dir = root.join(name.to_string());
        fs::create_dir_all(&dir).unwrap();
        let mut writer = PartWriter::create(&dir, name, schema, key, chunk.len as u64).unwrap();
        let mut offset = 0;
        while offset < chunk.len {
            let len = step.min(chunk.len - offset);
            writer.write(&chunk.slice(offset, len)).unwrap();
            offset += len;
        }
        let meta = writer.finish().unwrap();
        assert_eq!(meta.rows, chunk.len as u64);
        dir
    }

    pub(crate) fn assert_vectors_equal(a: &Vector, b: &Vector, what: &str) {
        assert_eq!(a.len(), b.len(), "{what}");
        assert_eq!(a.data_type(), b.data_type(), "{what}");
        for i in 0..a.len() {
            assert_eq!(a.get(i), b.get(i), "{what} row {i}");
        }
    }

    #[test]
    fn part_names_parse_and_cover() {
        let p = PartName::parse("all_3_7_2").unwrap();
        assert_eq!(p, PartName::new(3, 7, 2));
        assert_eq!(p.to_string(), "all_3_7_2");
        for bad in [
            "all_3_7",
            "all_7_3_0",
            "all_03_7_0",
            "all_3_7_0_1",
            "all_-1_2_0",
            "tmp_insert_3",
            "all_3_7_x",
            "all__7_0",
        ] {
            assert!(PartName::parse(bad).is_none(), "{bad}");
        }
        let a = PartName::new(1, 3, 1);
        assert!(a.covers(&PartName::new(1, 1, 0)));
        assert!(a.covers(&PartName::new(2, 3, 0)));
        assert!(!a.covers(&a));
        assert!(!a.covers(&PartName::new(3, 4, 0)));
        assert!(!PartName::new(2, 2, 0).covers(&a));
    }

    #[test]
    fn active_set_with_overlapping_parts() {
        let parts = [
            PartName::new(1, 1, 0),
            PartName::new(2, 2, 0),
            PartName::new(3, 3, 0),
            PartName::new(1, 2, 1),
            PartName::new(1, 3, 2),
            PartName::new(4, 4, 0),
            PartName::new(5, 6, 1),
            PartName::new(6, 6, 0),
        ];
        assert_eq!(
            active_parts(&parts),
            vec![
                PartName::new(1, 3, 2),
                PartName::new(4, 4, 0),
                PartName::new(5, 6, 1)
            ]
        );
        assert_eq!(
            covered_parts(&parts),
            vec![
                PartName::new(1, 1, 0),
                PartName::new(1, 2, 1),
                PartName::new(2, 2, 0),
                PartName::new(3, 3, 0),
                PartName::new(6, 6, 0),
            ]
        );
        // Same range, different level: they cover each other (§5.1 read literally).
        let twins = [PartName::new(1, 2, 1), PartName::new(1, 2, 2)];
        assert!(active_parts(&twins).is_empty());
    }

    #[test]
    fn every_type_round_trips_with_and_without_nulls() {
        let dir = TestDir::new("part-roundtrip");
        let schema = all_types_schema();
        let key = [7, 3];
        for (rows, step) in [
            (1usize, 1usize),
            (8192, 1000),
            (20_000, 20_000),
            (20_000, 8192),
        ] {
            let chunk = random_chunk(&schema, rows, rows as u64 + step as u64);
            let sorted = crate::sort::sort_chunk(&chunk, &key)
                .unwrap()
                .unwrap_or(chunk);
            let name = PartName::new(rows as u64, rows as u64 + step as u64, 0);
            let path = write_part(dir.path(), name, &schema, &key, &sorted, step);
            let part = PartReader::open(&path, &schema, &key).unwrap();
            assert_eq!(part.name(), name);
            assert_eq!(part.rows(), rows as u64);
            assert_eq!(part.granules(), granule_count(rows as u64));
            let granules = part.granules();
            assert_eq!(
                part.granule_rows(granules - 1),
                rows - (granules - 1) * GRANULE_ROWS
            );
            for c in 0..schema.len() {
                let read = part.read_column(c).unwrap();
                assert_vectors_equal(&read, sorted.column(c), &format!("{rows} rows, column {c}"));
                if !schema.field(c).nullable {
                    assert_eq!(read.null_count(), 0);
                }
                let mut expected = ColumnStats::new();
                expected.update_vector(sorted.column(c));
                assert_eq!(part.stats().columns[c], expected, "stats of column {c}");
            }
            for (j, &c) in key.iter().enumerate() {
                let index = &part.index()[j];
                assert_eq!(index.len(), granules + 1);
                for g in 0..granules {
                    assert_eq!(index.get(g), sorted.column(c).get(g * GRANULE_ROWS));
                }
                assert_eq!(index.get(granules), sorted.column(c).get(rows - 1));
            }
            assert_eq!(part.stats().key_prefix_hll.len(), 2);
            let positions = sample_positions(rows as u64);
            assert_eq!(part.sample().len, positions.len());
            for (i, &pos) in positions.iter().enumerate() {
                assert_eq!(part.sample().row(i), sorted.row(pos as usize));
            }
        }
    }

    #[test]
    fn long_strings_have_no_minmax_and_all_null_columns_neither() {
        let dir = TestDir::new("part-minmax");
        let schema = Schema::new(vec![
            Field::new("s", DataType::Varchar, false),
            Field::new("n", DataType::Int64, true),
        ]);
        let long = "x".repeat(300);
        let rows = vec![
            vec![Value::varchar(&long), Value::Null],
            vec![Value::varchar("a"), Value::Null],
        ];
        let chunk = Chunk::from_rows(&schema.data_types(), &rows).unwrap();
        let path = write_part(dir.path(), PartName::new(1, 1, 0), &schema, &[], &chunk, 2);
        let part = PartReader::open(&path, &schema, &[]).unwrap();
        let stats = &part.stats().columns;
        assert_eq!((stats[0].rows, stats[0].nulls), (2, 0));
        assert!(stats[0].min.is_none() && stats[0].max.is_none());
        assert_eq!((stats[1].rows, stats[1].nulls), (2, 2));
        assert!(stats[1].min.is_none());
        assert!(part.index().is_empty());
        assert_eq!(fs::read(path.join(PRIMARY_IDX)).unwrap().len(), 0);
    }

    #[test]
    fn written_files_follow_the_format() {
        let dir = TestDir::new("part-format");
        let schema = Schema::new(vec![
            Field::new("k", DataType::Int64, false),
            Field::new("s", DataType::Varchar, true),
        ]);
        let chunk = random_chunk(&schema, 10_000, 9);
        let sorted = crate::sort::sort_chunk(&chunk, &[0])
            .unwrap()
            .unwrap_or(chunk);
        let path = write_part(
            dir.path(),
            PartName::new(4, 9, 1),
            &schema,
            &[0],
            &sorted,
            3000,
        );
        let txt = fs::read_to_string(path.join(PART_TXT)).unwrap();
        assert_eq!(
            txt,
            "format=1\nrows=10000\ngranules=2\nindex_granularity=8192\nmin_block=4\n\
             max_block=9\nlevel=1\ncolumn=0\tk\tBIGINT\t0\ncolumn=1\ts\tVARCHAR\t1\n"
        );
        let checksums = fs::read_to_string(path.join(CHECKSUMS_TXT)).unwrap();
        let lines: Vec<&str> = checksums.lines().collect();
        assert_eq!(lines.len(), 9);
        let (last, rest) = lines.split_last().unwrap();
        let body: String = rest.iter().map(|l| format!("{l}\n")).collect();
        assert_eq!(*last, format!("self\t{:08x}", crc32c(body.as_bytes())));
        for line in rest {
            let f: Vec<&str> = line.split('\t').collect();
            let bytes = fs::read(path.join(f[0])).unwrap();
            assert_eq!(f[1], bytes.len().to_string());
            assert_eq!(f[2], format!("{:08x}", crc32c(&bytes)));
        }
        let marks = fs::read(path.join("c1.mrk")).unwrap();
        assert_eq!(marks.len(), 3 * 8);
        let last_mark = u64::from_le_bytes(marks[16..24].try_into().unwrap());
        assert_eq!(last_mark, fs::metadata(path.join("c1.bin")).unwrap().len());
        let stats = fs::read(path.join(STATS_BIN)).unwrap();
        let (body, tail) = stats.split_at(stats.len() - 4);
        assert_eq!(tail, crc32c(body).to_le_bytes());
        assert!(!path.join(OWNER_LOCK).exists());
    }

    fn open_and_read_all(path: &Path, schema: &Schema, key: &[usize]) -> Result<()> {
        let part = PartReader::open(path, schema, key)?;
        for c in 0..schema.len() {
            for g in 0..part.granules() {
                part.read_granule(c, g)?;
            }
        }
        Ok(())
    }

    #[test]
    fn a_changed_byte_in_any_file_is_corrupt() {
        let dir = TestDir::new("part-corrupt");
        let schema = Schema::new(vec![
            Field::new("k", DataType::Int64, false),
            Field::new("s", DataType::Varchar, true),
            Field::new("d", DataType::decimal(12, 2).unwrap(), true),
        ]);
        let key = [0, 1];
        let chunk = random_chunk(&schema, 9000, 21);
        let sorted = crate::sort::sort_chunk(&chunk, &key)
            .unwrap()
            .unwrap_or(chunk);
        let name = PartName::new(1, 1, 0);
        let path = write_part(dir.path(), name, &schema, &key, &sorted, 9000);
        open_and_read_all(&path, &schema, &key).unwrap();
        let mut files: Vec<String> = fs::read_dir(&path)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        files.sort();
        assert_eq!(files.len(), 11);
        for file in &files {
            let file_path = path.join(file);
            let original = fs::read(&file_path).unwrap();
            let len = original.len();
            let mut positions: Vec<usize> = (0..8).map(|k| k * len / 8).collect();
            positions.push(len - 1);
            positions.dedup();
            for at in positions {
                let mut bad = original.clone();
                bad[at] ^= 0x10;
                fs::write(&file_path, &bad).unwrap();
                let err = open_and_read_all(&path, &schema, &key).unwrap_err();
                assert!(
                    matches!(&err, VetaError::Corrupt { part, .. } if part == "all_1_1_0"),
                    "{file} byte {at}: {err:?}"
                );
            }
            fs::write(&file_path, &original).unwrap();
            open_and_read_all(&path, &schema, &key).unwrap();
        }
    }

    #[test]
    fn missing_or_resized_files_are_corrupt() {
        let dir = TestDir::new("part-missing");
        let schema = Schema::new(vec![Field::new("k", DataType::Int32, false)]);
        let chunk = random_chunk(&schema, 100, 2);
        let path = write_part(
            dir.path(),
            PartName::new(2, 2, 0),
            &schema,
            &[],
            &chunk,
            100,
        );
        let bin = path.join("c0.bin");
        let original = fs::read(&bin).unwrap();
        fs::write(&bin, &original[..original.len() - 1]).unwrap();
        let err = PartReader::open(&path, &schema, &[]).unwrap_err();
        assert!(matches!(err, VetaError::Corrupt { .. }), "{err:?}");
        fs::remove_file(&bin).unwrap();
        let err = PartReader::open(&path, &schema, &[]).unwrap_err();
        assert!(matches!(err, VetaError::Corrupt { .. }), "{err:?}");
        fs::write(&bin, &original).unwrap();
        PartReader::open(&path, &schema, &[]).unwrap();
        // A schema that differs from part.txt is corrupt too.
        let other = Schema::new(vec![Field::new("k", DataType::Int64, false)]);
        let err = PartReader::open(&path, &other, &[]).unwrap_err();
        assert!(matches!(err, VetaError::Corrupt { .. }), "{err:?}");
        // A vanished directory is an I/O error, not corruption.
        fs::remove_dir_all(&path).unwrap();
        let err = PartReader::open(&path, &schema, &[]).unwrap_err();
        assert!(matches!(err, VetaError::Io(_)), "{err:?}");
    }

    #[test]
    fn open_columns_keeps_only_the_requested_files() {
        let dir = TestDir::new("part-columns");
        let schema = all_types_schema();
        let chunk = random_chunk(&schema, 50, 4);
        let path = write_part(dir.path(), PartName::new(1, 1, 0), &schema, &[], &chunk, 50);
        let part = PartReader::open_columns(&path, &schema, &[], &[3]).unwrap();
        assert!(part.has_column(3));
        assert!(!part.has_column(0));
        assert_vectors_equal(
            &part.read_granule(3, 0).unwrap(),
            chunk.column(3),
            "column 3",
        );
        assert!(part.read_granule(0, 0).is_err());
        assert!(part.read_granule(3, 1).is_err());
    }

    #[test]
    fn writer_rejects_a_wrong_row_count() {
        let dir = TestDir::new("part-rows");
        let schema = Schema::new(vec![Field::new("k", DataType::Int32, false)]);
        let chunk = random_chunk(&schema, 10, 2);
        let name = PartName::new(1, 1, 0);
        let path = dir.path().join(name.to_string());
        fs::create_dir_all(&path).unwrap();
        assert!(PartWriter::create(&path, name, &schema, &[], 0).is_err());
        let mut writer = PartWriter::create(&path, name, &schema, &[], 11).unwrap();
        writer.write(&chunk).unwrap();
        assert!(writer.finish().is_err());
        let mut writer = PartWriter::create(&path, name, &schema, &[], 5).unwrap();
        assert!(writer.write(&chunk).is_err());
    }

    #[test]
    fn writing_column_by_column_produces_the_same_files() {
        let dir = TestDir::new("part-vertical");
        let schema = all_types_schema();
        let key = [14, 2];
        let rows = 20_000;
        let chunk = random_chunk(&schema, rows, 31);
        let sorted = crate::sort::sort_chunk(&chunk, &key)
            .unwrap()
            .unwrap_or(chunk);
        let name = PartName::new(1, 2, 1);
        let whole = write_part(
            &dir.path().join("whole"),
            name,
            &schema,
            &key,
            &sorted,
            3000,
        );

        let vertical = dir.path().join("vertical");
        fs::create_dir_all(&vertical).unwrap();
        let mut writer = PartWriter::create(&vertical, name, &schema, &key, rows as u64).unwrap();
        // The key rows may come in another order and the columns in other pieces.
        let half = rows / 2;
        for (offset, len) in [(half, rows - half), (0, half)] {
            let piece = sorted.slice(offset, len);
            let columns: Vec<&Vector> = key.iter().map(|&c| piece.column(c)).collect();
            writer.add_key_rows(&columns).unwrap();
        }
        for column in (0..schema.len()).rev() {
            for (offset, len) in [
                (0, GRANULE_ROWS),
                (GRANULE_ROWS, 1),
                (GRANULE_ROWS + 1, rows - GRANULE_ROWS - 1),
            ] {
                writer
                    .write_column(column, &sorted.column(column).slice(offset, len))
                    .unwrap();
            }
        }
        writer.finish().unwrap();

        for entry in fs::read_dir(&whole).unwrap() {
            let file = entry.unwrap().file_name();
            assert_eq!(
                fs::read(whole.join(&file)).unwrap(),
                fs::read(vertical.join(&file)).unwrap(),
                "{file:?}"
            );
        }
    }

    #[test]
    fn column_writes_are_checked() {
        let dir = TestDir::new("part-vertical-checks");
        let schema = Schema::new(vec![
            Field::new("k", DataType::Int32, false),
            Field::new("v", DataType::Int64, true),
        ]);
        let chunk = random_chunk(&schema, 10, 2);
        let name = PartName::new(1, 1, 0);
        let path = dir.path().join(name.to_string());
        fs::create_dir_all(&path).unwrap();
        let mut writer = PartWriter::create(&path, name, &schema, &[0], 10).unwrap();
        assert!(writer.write_column(2, chunk.column(0)).is_err());
        assert!(writer.write_column(0, chunk.column(1)).is_err());
        assert!(writer.add_key_rows(&[]).is_err());
        assert!(writer.add_key_rows(&[chunk.column(1)]).is_err());
        writer.write_column(0, chunk.column(0)).unwrap();
        assert!(writer.write_column(0, chunk.column(0)).is_err());
        // A column or the key rows missing is an error at the end.
        let mut writer = PartWriter::create(&path, name, &schema, &[0], 10).unwrap();
        writer.add_key_rows(&[chunk.column(0)]).unwrap();
        writer.write_column(0, chunk.column(0)).unwrap();
        assert!(writer.finish().is_err());
        let mut writer = PartWriter::create(&path, name, &schema, &[0], 10).unwrap();
        writer.write_column(0, chunk.column(0)).unwrap();
        writer.write_column(1, chunk.column(1)).unwrap();
        assert!(writer.finish().is_err());
    }
}
