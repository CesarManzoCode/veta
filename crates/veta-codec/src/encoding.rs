//! Column encodings of a block payload (contract §8.1).
//!
//! This layer encodes only the values of a vector; the validity map belongs to the block
//! (`veta-storage`, §5.3). Values under NULL are normalized to zero, `false` or the empty
//! string before encoding, so `decode(encode(x))` returns `x`'s values with those slots
//! normalized and no validity.
//!
//! Formats (all integers little-endian):
//!
//! - `Plain`: fixed-width values in LE (`Int16` 2 bytes, `Int32`/`Date` 4, `Int64`,
//!   `Timestamp`, `Float64` and decimals up to precision 18 8, `Int128` and wider decimals
//!   16); booleans as bits LSB-first in `ceil(rows/8)` bytes; strings as their lengths with
//!   `ForBitpack` followed by the concatenated bytes.
//! - `ForBitpack`: `min` (8 bytes, or 16 for 128-bit types) + `bit_width u8` + `v − min`
//!   bit-packed ([`crate::bitpack`]). A 128-bit column whose range does not fit in 64 bits
//!   cannot use it.
//! - `DeltaBitpack`: first value (8 or 16 bytes; 0 for an empty vector) + `ForBitpack` of
//!   the `rows − 1` consecutive differences, wrapping at the physical width (64 or 128
//!   bits).
//! - `Dict`: `u32` dictionary length + dictionary (sub-encoding byte, then the entries in
//!   order of first appearance with `Plain` (0) or, for integers, `ForBitpack` (1)) + codes
//!   with `ForBitpack` (8-byte `min`).
//! - `Rle`: `u32` run count + run values (sub-encoding byte as in `Dict`) + run lengths
//!   with `ForBitpack` (8-byte `min`).
//!
//! `ForBitpack` and `DeltaBitpack` apply to integers, dates, timestamps and decimals;
//! `Dict` and `Rle` to integers (`Int16`, `Int32`, `Int64`, `Int128`) and strings; `Plain`
//! to every type. Decoding never panics on malformed input: truncated bytes, trailing
//! bytes, invalid widths, codes outside the dictionary, run lengths that do not add up to
//! `rows` and values outside the column type are all `Err`.

use std::collections::HashMap;

use veta_core::types::MAX_DECIMAL64_PRECISION;
use veta_core::{DataType, Result, Vector, VectorData, VetaError};

use crate::bitpack;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Encoding {
    Plain,
    ForBitpack,
    DeltaBitpack,
    Dict,
    Rle,
}

impl Encoding {
    /// Every encoding, in id order.
    pub const ALL: [Encoding; 5] = [
        Encoding::Plain,
        Encoding::ForBitpack,
        Encoding::DeltaBitpack,
        Encoding::Dict,
        Encoding::Rle,
    ];

    /// The `encoding` byte of the block header (§5.3, §8.1).
    pub fn id(self) -> u8 {
        match self {
            Encoding::Plain => 0,
            Encoding::ForBitpack => 1,
            Encoding::DeltaBitpack => 2,
            Encoding::Dict => 3,
            Encoding::Rle => 4,
        }
    }

    pub fn from_id(id: u8) -> Result<Encoding> {
        Encoding::ALL
            .into_iter()
            .find(|e| e.id() == id)
            .ok_or_else(|| malformed(format!("unknown encoding id {id}")))
    }

    pub fn name(self) -> &'static str {
        match self {
            Encoding::Plain => "Plain",
            Encoding::ForBitpack => "ForBitpack",
            Encoding::DeltaBitpack => "DeltaBitpack",
            Encoding::Dict => "Dict",
            Encoding::Rle => "Rle",
        }
    }

    /// Whether the encoding is defined for `data_type`. `ForBitpack` and `DeltaBitpack` on a
    /// 128-bit column additionally need the values to fit, which only [`encode_with`] knows.
    pub fn applies_to(self, data_type: &DataType) -> bool {
        match self {
            Encoding::Plain => true,
            Encoding::ForBitpack | Encoding::DeltaBitpack => IntKind::of(data_type).is_some(),
            Encoding::Dict | Encoding::Rle => {
                data_type.is_integer() || *data_type == DataType::Varchar
            }
        }
    }
}

fn malformed(reason: impl Into<String>) -> VetaError {
    VetaError::corrupt("", reason)
}

/// Physical integer width of an integer-like type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IntKind {
    I16,
    I32,
    I64,
    I128,
}

impl IntKind {
    fn of(data_type: &DataType) -> Option<IntKind> {
        match data_type {
            DataType::Int16 => Some(IntKind::I16),
            DataType::Int32 | DataType::Date => Some(IntKind::I32),
            DataType::Int64 | DataType::Timestamp => Some(IntKind::I64),
            DataType::Int128 => Some(IntKind::I128),
            DataType::Decimal { precision, .. } => Some(if *precision <= MAX_DECIMAL64_PRECISION {
                IntKind::I64
            } else {
                IntKind::I128
            }),
            DataType::Boolean | DataType::Float64 | DataType::Varchar => None,
        }
    }

    fn wide(self) -> bool {
        self == IntKind::I128
    }

    fn plain_width(self) -> usize {
        match self {
            IntKind::I16 => 2,
            IntKind::I32 => 4,
            IntKind::I64 => 8,
            IntKind::I128 => 16,
        }
    }

    fn fits(self, v: i128) -> bool {
        match self {
            IntKind::I16 => i16::try_from(v).is_ok(),
            IntKind::I32 => i32::try_from(v).is_ok(),
            IntKind::I64 => i64::try_from(v).is_ok(),
            IntKind::I128 => true,
        }
    }
}

/// The values of a vector with NULL slots normalized, in a type-independent shape.
enum Column<'a> {
    Bool(Vec<bool>),
    Float(Vec<f64>),
    Int(IntKind, Vec<i128>),
    Str(Vec<&'a [u8]>),
}

fn normalize(vector: &Vector) -> Column<'_> {
    let valid = |i: usize| vector.is_valid(i);
    fn ints<T: Copy + Into<i128>>(v: &[T], valid: impl Fn(usize) -> bool) -> Vec<i128> {
        v.iter()
            .enumerate()
            .map(|(i, &x)| if valid(i) { x.into() } else { 0 })
            .collect()
    }
    match &vector.data {
        VectorData::Boolean(v) => {
            Column::Bool(v.iter().enumerate().map(|(i, &b)| b && valid(i)).collect())
        }
        VectorData::Float64(v) => Column::Float(
            v.iter()
                .enumerate()
                .map(|(i, &f)| if valid(i) { f } else { 0.0 })
                .collect(),
        ),
        VectorData::Int16(v) => Column::Int(IntKind::I16, ints(v, valid)),
        VectorData::Int32(v) | VectorData::Date(v) => Column::Int(IntKind::I32, ints(v, valid)),
        VectorData::Int64(v) | VectorData::Timestamp(v) => {
            Column::Int(IntKind::I64, ints(v, valid))
        }
        VectorData::Decimal64 { values, .. } => Column::Int(IntKind::I64, ints(values, valid)),
        VectorData::Int128(v) => Column::Int(IntKind::I128, ints(v, valid)),
        VectorData::Decimal128 { values, .. } => Column::Int(IntKind::I128, ints(values, valid)),
        VectorData::Varchar { offsets, bytes } => Column::Str(
            (0..offsets.len() - 1)
                .map(|i| {
                    if valid(i) {
                        &bytes[offsets[i] as usize..offsets[i + 1] as usize]
                    } else {
                        &[][..]
                    }
                })
                .collect(),
        ),
    }
}

/// Encodes `vector` with every applicable encoding and keeps the smallest (the lowest id on
/// a tie). `Plain` always applies, so there is always a result.
pub fn encode(vector: &Vector) -> (Encoding, Vec<u8>) {
    let data_type = vector.data_type();
    let column = normalize(vector);
    let mut best: Option<(Encoding, Vec<u8>)> = None;
    for encoding in Encoding::ALL {
        if !encoding.applies_to(&data_type) {
            continue;
        }
        if let Some(bytes) = encode_column(&column, encoding)
            && best.as_ref().is_none_or(|(_, b)| bytes.len() < b.len())
        {
            best = Some((encoding, bytes));
        }
    }
    match best {
        Some(best) => best,
        None => (
            Encoding::Plain,
            encode_column(&column, Encoding::Plain).unwrap_or_default(),
        ),
    }
}

/// Encodes `vector` with `encoding`. Errors if the encoding does not apply to the type, or
/// to these values (`ForBitpack`/`DeltaBitpack` on a 128-bit range wider than 64 bits).
pub fn encode_with(vector: &Vector, encoding: Encoding) -> Result<Vec<u8>> {
    let data_type = vector.data_type();
    let inapplicable = || {
        VetaError::internal(format!(
            "{} does not apply to this {data_type} vector",
            encoding.name()
        ))
    };
    if !encoding.applies_to(&data_type) {
        return Err(inapplicable());
    }
    encode_column(&normalize(vector), encoding).ok_or_else(inapplicable)
}

fn encode_column(column: &Column<'_>, encoding: Encoding) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    match (column, encoding) {
        (Column::Bool(v), Encoding::Plain) => put_bools(v, &mut out),
        (Column::Float(v), Encoding::Plain) => {
            for f in v {
                out.extend_from_slice(&f.to_le_bytes());
            }
        }
        (Column::Int(kind, v), Encoding::Plain) => put_plain_ints(*kind, v, &mut out),
        (Column::Int(kind, v), Encoding::ForBitpack) => put_for(kind.wide(), v, &mut out)?,
        (Column::Int(kind, v), Encoding::DeltaBitpack) => put_delta(*kind, v, &mut out)?,
        (Column::Int(kind, v), Encoding::Dict) => {
            let (dict, codes) = dictionary(v);
            put_u32(dict.len(), &mut out);
            put_ints_sub(*kind, &dict, &mut out);
            put_codes(&codes, &mut out);
        }
        (Column::Int(kind, v), Encoding::Rle) => {
            let (values, lengths) = runs(v);
            put_u32(values.len(), &mut out);
            put_ints_sub(*kind, &values, &mut out);
            put_codes(&lengths, &mut out);
        }
        (Column::Str(v), Encoding::Plain) => put_plain_strs(v, &mut out),
        (Column::Str(v), Encoding::Dict) => {
            let (dict, codes) = dictionary(v);
            put_u32(dict.len(), &mut out);
            out.push(Encoding::Plain.id());
            put_plain_strs(&dict, &mut out);
            put_codes(&codes, &mut out);
        }
        (Column::Str(v), Encoding::Rle) => {
            let (values, lengths) = runs(v);
            put_u32(values.len(), &mut out);
            out.push(Encoding::Plain.id());
            put_plain_strs(&values, &mut out);
            put_codes(&lengths, &mut out);
        }
        _ => return None,
    }
    Some(out)
}

// ---------------------------------------------------------------------------------------
// Writers.

fn put_u32(n: usize, out: &mut Vec<u8>) {
    // A block holds one granule (8192 rows) or a sample, far below `u32::MAX`.
    let n = u32::try_from(n).expect("count exceeds u32");
    out.extend_from_slice(&n.to_le_bytes());
}

fn put_bools(v: &[bool], out: &mut Vec<u8>) {
    let start = out.len();
    out.resize(start + v.len().div_ceil(8), 0);
    for (i, &b) in v.iter().enumerate() {
        if b {
            out[start + i / 8] |= 1 << (i % 8);
        }
    }
}

fn put_plain_ints(kind: IntKind, v: &[i128], out: &mut Vec<u8>) {
    let width = kind.plain_width();
    for &x in v {
        out.extend_from_slice(&x.to_le_bytes()[..width]);
    }
}

fn put_min(wide: bool, min: i128, out: &mut Vec<u8>) {
    if wide {
        out.extend_from_slice(&min.to_le_bytes());
    } else {
        out.extend_from_slice(&(min as i64).to_le_bytes());
    }
}

/// `ForBitpack`; `None` if the range does not fit in 64 bits (only possible when `wide`).
fn put_for(wide: bool, v: &[i128], out: &mut Vec<u8>) -> Option<()> {
    let min = v.iter().copied().min().unwrap_or(0);
    let max = v.iter().copied().max().unwrap_or(0);
    let range = u64::try_from(max.checked_sub(min)?).ok()?;
    let width = bitpack::width_for(range);
    put_min(wide, min, out);
    out.push(width);
    let offsets: Vec<u64> = v.iter().map(|&x| (x - min) as u64).collect();
    bitpack::pack(&offsets, width, out);
    Some(())
}

/// Difference `b − a` wrapping at the physical width.
fn wrapping_diff(wide: bool, a: i128, b: i128) -> i128 {
    if wide {
        b.wrapping_sub(a)
    } else {
        (b as i64).wrapping_sub(a as i64) as i128
    }
}

fn put_delta(kind: IntKind, v: &[i128], out: &mut Vec<u8>) -> Option<()> {
    let wide = kind.wide();
    let diffs: Vec<i128> = v
        .windows(2)
        .map(|w| wrapping_diff(wide, w[0], w[1]))
        .collect();
    let mut body = Vec::new();
    put_min(wide, v.first().copied().unwrap_or(0), &mut body);
    put_for(wide, &diffs, &mut body)?;
    out.extend_from_slice(&body);
    Some(())
}

/// Codes, run lengths and string lengths: non-negative and below 2⁶³, so `ForBitpack` with
/// an 8-byte `min` always applies.
fn put_codes(v: &[u64], out: &mut Vec<u8>) {
    let wide: Vec<i128> = v.iter().map(|&x| x as i128).collect();
    put_for(false, &wide, out).expect("codes fit in 64 bits");
}

/// Dictionary or run values of an integer column: a sub-encoding byte and the smaller of
/// `Plain` and `ForBitpack`.
fn put_ints_sub(kind: IntKind, v: &[i128], out: &mut Vec<u8>) {
    let mut plain = Vec::new();
    put_plain_ints(kind, v, &mut plain);
    let mut packed = Vec::new();
    if put_for(kind.wide(), v, &mut packed).is_some() && packed.len() < plain.len() {
        out.push(Encoding::ForBitpack.id());
        out.extend_from_slice(&packed);
    } else {
        out.push(Encoding::Plain.id());
        out.extend_from_slice(&plain);
    }
}

fn put_plain_strs(v: &[&[u8]], out: &mut Vec<u8>) {
    let lengths: Vec<u64> = v.iter().map(|s| s.len() as u64).collect();
    put_codes(&lengths, out);
    for s in v {
        out.extend_from_slice(s);
    }
}

/// Distinct values in order of first appearance and the code of every row.
fn dictionary<T: Copy + Eq + std::hash::Hash>(v: &[T]) -> (Vec<T>, Vec<u64>) {
    let mut index: HashMap<T, u64> = HashMap::new();
    let mut dict = Vec::new();
    let codes = v
        .iter()
        .map(|&x| {
            *index.entry(x).or_insert_with(|| {
                dict.push(x);
                dict.len() as u64 - 1
            })
        })
        .collect();
    (dict, codes)
}

/// Value and length of every maximal run of equal values.
fn runs<T: Copy + PartialEq>(v: &[T]) -> (Vec<T>, Vec<u64>) {
    let mut values: Vec<T> = Vec::new();
    let mut lengths: Vec<u64> = Vec::new();
    for &x in v {
        let n = values.len();
        if n > 0 && values[n - 1] == x {
            lengths[n - 1] += 1;
        } else {
            values.push(x);
            lengths.push(1);
        }
    }
    (values, lengths)
}

// ---------------------------------------------------------------------------------------
// Reader.

/// A little-endian signed integer of 1 to 16 bytes, sign-extended to `i128`.
fn sign_extend(b: &[u8]) -> i128 {
    let fill = if b[b.len() - 1] & 0x80 != 0 { 0xFF } else { 0 };
    let mut full = [fill; 16];
    full[..b.len()].copy_from_slice(b);
    i128::from_le_bytes(full)
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .filter(|&end| end <= self.buf.len())
            .ok_or_else(|| {
                malformed(format!(
                    "encoded data truncated: need {n} bytes at offset {}, have {}",
                    self.pos,
                    self.buf.len() - self.pos
                ))
            })?;
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// The `min` or first value of `ForBitpack`/`DeltaBitpack`: 16 bytes if `wide`, else 8.
    fn min(&mut self, wide: bool) -> Result<i128> {
        Ok(sign_extend(self.take(if wide { 16 } else { 8 })?))
    }

    fn finish(&self) -> Result<()> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(malformed(format!(
                "{} trailing bytes after encoded data",
                self.buf.len() - self.pos
            )))
        }
    }
}

// ---------------------------------------------------------------------------------------
// Decoders.

/// Decodes `rows` values of `data_type` encoded with `encoding`. The result has no validity
/// map. Every malformed input is an `Err`.
pub fn decode(
    encoding: Encoding,
    bytes: &[u8],
    data_type: &DataType,
    rows: usize,
) -> Result<Vector> {
    if !encoding.applies_to(data_type) {
        return Err(malformed(format!(
            "encoding {} is not valid for {data_type}",
            encoding.name()
        )));
    }
    let mut r = Reader::new(bytes);
    let data = match data_type {
        DataType::Boolean => {
            let b = r.take(rows.div_ceil(8))?;
            VectorData::Boolean((0..rows).map(|i| (b[i / 8] >> (i % 8)) & 1 == 1).collect())
        }
        DataType::Float64 => {
            let b = r.take(byte_len(rows, 8)?)?;
            VectorData::Float64(
                b.chunks_exact(8)
                    .map(|c| f64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]))
                    .collect(),
            )
        }
        DataType::Varchar => strings_to_data(&get_strs(encoding, &mut r, rows)?)?,
        _ => {
            let kind = IntKind::of(data_type)
                .ok_or_else(|| VetaError::internal(format!("no integer layout for {data_type}")))?;
            let values = get_ints(encoding, kind, &mut r, rows)?;
            ints_to_data(data_type, kind, values)?
        }
    };
    r.finish()?;
    Ok(Vector::from_data(data))
}

fn byte_len(count: usize, width: usize) -> Result<usize> {
    count
        .checked_mul(width)
        .ok_or_else(|| malformed(format!("{count} values overflow")))
}

/// A `u32` element count that cannot exceed `rows` (dictionary entries, runs).
fn get_count(r: &mut Reader<'_>, rows: usize, what: &str) -> Result<usize> {
    let n = r.u32()? as usize;
    if n > rows {
        return Err(malformed(format!("{n} {what} for {rows} rows")));
    }
    Ok(n)
}

fn get_plain_ints(kind: IntKind, r: &mut Reader<'_>, count: usize) -> Result<Vec<i128>> {
    let width = kind.plain_width();
    let bytes = r.take(byte_len(count, width)?)?;
    Ok(bytes.chunks_exact(width).map(sign_extend).collect())
}

fn get_for(wide: bool, r: &mut Reader<'_>, count: usize) -> Result<Vec<i128>> {
    let min = r.min(wide)?;
    let width = r.u8()?;
    let len = bitpack::packed_len(count, width)?;
    let offsets = bitpack::unpack(r.take(len)?, count, width)?;
    offsets
        .into_iter()
        .map(|u| {
            min.checked_add(u as i128)
                .ok_or_else(|| malformed("ForBitpack value overflows 128 bits"))
        })
        .collect()
}

fn get_delta(kind: IntKind, r: &mut Reader<'_>, count: usize) -> Result<Vec<i128>> {
    let wide = kind.wide();
    let first = r.min(wide)?;
    let diffs = get_for(wide, r, count.saturating_sub(1))?;
    if count == 0 {
        return Ok(Vec::new());
    }
    let mut out = Vec::with_capacity(count);
    out.push(first);
    let mut acc = first;
    for d in diffs {
        acc = if wide {
            acc.wrapping_add(d)
        } else {
            (acc as i64).wrapping_add(d as i64) as i128
        };
        out.push(acc);
    }
    Ok(out)
}

/// Codes, run lengths or string lengths: `ForBitpack` with an 8-byte `min`, each value in
/// `[0, limit)`.
fn get_codes(r: &mut Reader<'_>, count: usize, limit: u64, what: &str) -> Result<Vec<u64>> {
    get_for(false, r, count)?
        .into_iter()
        .map(|c| {
            u64::try_from(c)
                .ok()
                .filter(|&c| c < limit)
                .ok_or_else(|| malformed(format!("{what} {c} out of range [0, {limit})")))
        })
        .collect()
}

fn get_ints_sub(kind: IntKind, r: &mut Reader<'_>, count: usize) -> Result<Vec<i128>> {
    let tag = r.u8()?;
    match Encoding::from_id(tag)? {
        Encoding::Plain => get_plain_ints(kind, r, count),
        Encoding::ForBitpack => get_for(kind.wide(), r, count),
        other => Err(malformed(format!(
            "invalid sub-encoding {} for dictionary or run values",
            other.name()
        ))),
    }
}

/// Expands runs: `lengths` must be ≥ 1 each and add up to `rows`.
fn expand_runs<T: Copy>(values: &[T], lengths: &[u64], rows: usize) -> Result<Vec<T>> {
    let mut total: u64 = 0;
    for &len in lengths {
        if len == 0 {
            return Err(malformed("run of length 0"));
        }
        total = total.saturating_add(len);
    }
    if total != rows as u64 {
        return Err(malformed(format!(
            "runs cover {total} rows, block has {rows}"
        )));
    }
    let mut out = Vec::with_capacity(rows);
    for (&value, &len) in values.iter().zip(lengths) {
        out.extend(std::iter::repeat_n(value, len as usize));
    }
    Ok(out)
}

fn get_ints(
    encoding: Encoding,
    kind: IntKind,
    r: &mut Reader<'_>,
    rows: usize,
) -> Result<Vec<i128>> {
    match encoding {
        Encoding::Plain => get_plain_ints(kind, r, rows),
        Encoding::ForBitpack => get_for(kind.wide(), r, rows),
        Encoding::DeltaBitpack => get_delta(kind, r, rows),
        Encoding::Dict => {
            let n = get_count(r, rows, "dictionary entries")?;
            let dict = get_ints_sub(kind, r, n)?;
            let codes = get_codes(r, rows, n as u64, "dictionary code")?;
            Ok(codes.into_iter().map(|c| dict[c as usize]).collect())
        }
        Encoding::Rle => {
            let n = get_count(r, rows, "runs")?;
            let values = get_ints_sub(kind, r, n)?;
            let lengths = get_codes(r, n, u64::MAX, "run length")?;
            expand_runs(&values, &lengths, rows)
        }
    }
}

fn get_plain_strs<'a>(r: &mut Reader<'a>, count: usize) -> Result<Vec<&'a [u8]>> {
    let lengths = get_codes(r, count, u64::MAX, "string length")?;
    let mut out = Vec::with_capacity(count);
    for len in lengths {
        let len = usize::try_from(len).map_err(|_| malformed("string length overflows"))?;
        out.push(r.take(len)?);
    }
    Ok(out)
}

fn get_strs<'a>(encoding: Encoding, r: &mut Reader<'a>, rows: usize) -> Result<Vec<&'a [u8]>> {
    let sub = |r: &mut Reader<'a>, n: usize| -> Result<Vec<&'a [u8]>> {
        match r.u8()? {
            0 => get_plain_strs(r, n),
            tag => Err(malformed(format!("invalid string sub-encoding {tag}"))),
        }
    };
    match encoding {
        Encoding::Plain => get_plain_strs(r, rows),
        Encoding::Dict => {
            let n = get_count(r, rows, "dictionary entries")?;
            let dict = sub(r, n)?;
            let codes = get_codes(r, rows, n as u64, "dictionary code")?;
            Ok(codes.into_iter().map(|c| dict[c as usize]).collect())
        }
        Encoding::Rle => {
            let n = get_count(r, rows, "runs")?;
            let values = sub(r, n)?;
            let lengths = get_codes(r, n, u64::MAX, "run length")?;
            expand_runs(&values, &lengths, rows)
        }
        Encoding::ForBitpack | Encoding::DeltaBitpack => Err(malformed(format!(
            "encoding {} is not valid for VARCHAR",
            encoding.name()
        ))),
    }
}

fn strings_to_data(strings: &[&[u8]]) -> Result<VectorData> {
    let total: usize = strings.iter().map(|s| s.len()).sum();
    if u32::try_from(total).is_err() {
        return Err(malformed("decoded strings exceed 4 GiB"));
    }
    let mut offsets = Vec::with_capacity(strings.len() + 1);
    let mut bytes = Vec::with_capacity(total);
    offsets.push(0u32);
    for s in strings {
        bytes.extend_from_slice(s);
        offsets.push(bytes.len() as u32);
    }
    Ok(VectorData::Varchar { offsets, bytes })
}

fn ints_to_data(data_type: &DataType, kind: IntKind, values: Vec<i128>) -> Result<VectorData> {
    if let Some(bad) = values.iter().find(|&&v| !kind.fits(v)) {
        return Err(malformed(format!(
            "decoded value {bad} does not fit {data_type}"
        )));
    }
    let narrow = |values: Vec<i128>| -> Vec<i64> { values.into_iter().map(|v| v as i64).collect() };
    let narrow32 =
        |values: Vec<i128>| -> Vec<i32> { values.into_iter().map(|v| v as i32).collect() };
    Ok(match *data_type {
        DataType::Int16 => VectorData::Int16(values.into_iter().map(|v| v as i16).collect()),
        DataType::Int32 => VectorData::Int32(narrow32(values)),
        DataType::Date => VectorData::Date(narrow32(values)),
        DataType::Int64 => VectorData::Int64(narrow(values)),
        DataType::Timestamp => VectorData::Timestamp(narrow(values)),
        DataType::Int128 => VectorData::Int128(values),
        DataType::Decimal { precision, scale } if kind == IntKind::I64 => VectorData::Decimal64 {
            values: narrow(values),
            precision,
            scale,
        },
        DataType::Decimal { precision, scale } => VectorData::Decimal128 {
            values,
            precision,
            scale,
        },
        DataType::Boolean | DataType::Float64 | DataType::Varchar => {
            return Err(VetaError::internal(format!(
                "{data_type} is not an integer layout"
            )));
        }
    })
}

#[cfg(test)]
mod tests;
