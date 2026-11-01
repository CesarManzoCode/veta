//! Parquet reader for flat schemas (contract §9): schema mapping to veta types, column
//! chunks decoded straight into `Vector` buffers, a `Chunk` iterator of up to
//! [`CHUNK_ROWS`] rows per row group, and row-group statistics for skipping (§11.4).
//!
//! Pages: DICTIONARY_PAGE, DATA_PAGE (v1: levels with a 4-byte length prefix, inside the
//! compression) and DATA_PAGE_V2 (levels outside the compression, `is_compressed`); other
//! page types are skipped. A page whose header carries `crc` is checked with CRC-32 IEEE
//! over its stored (compressed) bytes. BIT_PACKED is rejected when a page uses it, not when
//! the chunk lists it: old writers list it for repetition levels of flat columns, which
//! hold no data.

use crate::encodings::{self, ByteArrays};
use crate::footer::read_footer;
use crate::invalid;
use crate::meta::{
    ColumnMetaData, CompressionCodec, ConvertedType, Encoding, FieldRepetitionType, FileMetaData,
    LogicalType, PageHeader, PageType, SchemaElement, TimeUnit, Type, decode,
};
use std::borrow::Cow;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;
use veta_codec::snappy;
use veta_core::crc::crc32;
use veta_core::types::{MAX_DECIMAL_PRECISION, MAX_DECIMAL64_PRECISION};
use veta_core::{
    Bitmap, Chunk, DataType, Field, Result, Schema, Value, Vector, VectorData, VetaError,
};

/// Rows per `Chunk` yielded by [`ChunkIter`] (the vector size of contract §12).
pub const CHUNK_ROWS: usize = 8192;

/// Physical storage of a column, with the width of FIXED_LEN_BYTE_ARRAY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Physical {
    Boolean,
    Int32,
    Int64,
    Float,
    Double,
    ByteArray,
    Fixed(usize),
}

impl Physical {
    fn parquet_type(self) -> Type {
        match self {
            Physical::Boolean => Type::Boolean,
            Physical::Int32 => Type::Int32,
            Physical::Int64 => Type::Int64,
            Physical::Float => Type::Float,
            Physical::Double => Type::Double,
            Physical::ByteArray => Type::ByteArray,
            Physical::Fixed(_) => Type::FixedLenByteArray,
        }
    }
}

/// How physical values become values of the veta type (contract §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Convert {
    Boolean,
    /// INT32 + INT(8, signed).
    Int8ToInt16,
    /// INT32 + INT(8, unsigned).
    UInt8ToInt16,
    /// INT32 + INT(16, signed).
    Int16,
    /// INT32 + INT(16, unsigned).
    UInt16ToInt32,
    Int32,
    /// INT32 + INT(32, unsigned).
    UInt32ToInt64,
    Date,
    Int64,
    /// INT64 + INT(64, unsigned): only `Int128` holds every value.
    UInt64ToInt128,
    /// INT64 + TIMESTAMP, `per_second` units per second, floored to seconds.
    Timestamp {
        per_second: i64,
    },
    /// DECIMAL over INT32, INT64 or FIXED_LEN_BYTE_ARRAY (big-endian two's complement).
    Decimal {
        precision: u8,
        scale: u8,
    },
    Float64,
    Varchar,
}

impl Convert {
    fn data_type(self) -> DataType {
        match self {
            Convert::Boolean => DataType::Boolean,
            Convert::Int8ToInt16 | Convert::UInt8ToInt16 | Convert::Int16 => DataType::Int16,
            Convert::UInt16ToInt32 | Convert::Int32 => DataType::Int32,
            Convert::UInt32ToInt64 | Convert::Int64 => DataType::Int64,
            Convert::UInt64ToInt128 => DataType::Int128,
            Convert::Date => DataType::Date,
            Convert::Timestamp { .. } => DataType::Timestamp,
            Convert::Decimal { precision, scale } => DataType::Decimal { precision, scale },
            Convert::Float64 => DataType::Float64,
            Convert::Varchar => DataType::Varchar,
        }
    }

    /// Legacy `min`/`max` statistics (fields 1 and 2) are trusted only for signed integers
    /// and dates (contract §9): their sort order was undefined for every other type.
    fn trusts_legacy_stats(self) -> bool {
        matches!(
            self,
            Convert::Int8ToInt16 | Convert::Int16 | Convert::Int32 | Convert::Int64 | Convert::Date
        )
    }
}

#[derive(Debug, Clone)]
struct ColumnInfo {
    name: String,
    physical: Physical,
    convert: Convert,
    nullable: bool,
}

/// The annotation of a leaf, from `logicalType` or, when absent, `converted_type`.
enum Annotation {
    None,
    Int { bits: i8, signed: bool },
    Date,
    Timestamp(TimeUnit),
    Decimal { precision: i32, scale: i32 },
    String,
    Other(String),
}

fn annotation(el: &SchemaElement) -> Result<Annotation> {
    if let Some(logical) = el.logical_type {
        return Ok(match logical {
            LogicalType::String | LogicalType::Enum => Annotation::String,
            LogicalType::Decimal { scale, precision } => Annotation::Decimal { precision, scale },
            LogicalType::Date => Annotation::Date,
            LogicalType::Timestamp { unit, .. } => Annotation::Timestamp(unit),
            LogicalType::Integer {
                bit_width,
                is_signed,
            } => Annotation::Int {
                bits: bit_width,
                signed: is_signed,
            },
            LogicalType::Unknown(id) => Annotation::Other(format!("logical type {id}")),
        });
    }
    let Some(converted) = el.converted_type else {
        return Ok(Annotation::None);
    };
    Ok(match converted {
        ConvertedType::Utf8 | ConvertedType::Enum => Annotation::String,
        ConvertedType::Decimal => Annotation::Decimal {
            precision: el
                .precision
                .ok_or_else(|| invalid(format!("DECIMAL column {} without precision", el.name)))?,
            scale: el.scale.unwrap_or(0),
        },
        ConvertedType::Date => Annotation::Date,
        ConvertedType::TimestampMillis => Annotation::Timestamp(TimeUnit::Millis),
        ConvertedType::TimestampMicros => Annotation::Timestamp(TimeUnit::Micros),
        ConvertedType::Int8 => Annotation::Int {
            bits: 8,
            signed: true,
        },
        ConvertedType::Int16 => Annotation::Int {
            bits: 16,
            signed: true,
        },
        ConvertedType::Int32 => Annotation::Int {
            bits: 32,
            signed: true,
        },
        ConvertedType::Int64 => Annotation::Int {
            bits: 64,
            signed: true,
        },
        ConvertedType::Uint8 => Annotation::Int {
            bits: 8,
            signed: false,
        },
        ConvertedType::Uint16 => Annotation::Int {
            bits: 16,
            signed: false,
        },
        ConvertedType::Uint32 => Annotation::Int {
            bits: 32,
            signed: false,
        },
        ConvertedType::Uint64 => Annotation::Int {
            bits: 64,
            signed: false,
        },
        other => Annotation::Other(other.name()),
    })
}

/// Maps a leaf of the schema to its physical storage and veta type (contract §9).
fn map_column(el: &SchemaElement) -> Result<(Physical, Convert)> {
    let name = &el.name;
    let unsupported =
        |what: String| VetaError::unsupported(format!("parquet column {name}: {what}"));
    let physical = match el.type_ {
        Some(Type::Boolean) => Physical::Boolean,
        Some(Type::Int32) => Physical::Int32,
        Some(Type::Int64) => Physical::Int64,
        Some(Type::Float) => Physical::Float,
        Some(Type::Double) => Physical::Double,
        Some(Type::ByteArray) => Physical::ByteArray,
        Some(Type::FixedLenByteArray) => {
            let len = el.type_length.unwrap_or(0);
            if len <= 0 {
                return Err(invalid(format!(
                    "FIXED_LEN_BYTE_ARRAY column {name} with type_length {len}"
                )));
            }
            Physical::Fixed(len as usize)
        }
        Some(Type::Int96) => return Err(unsupported("INT96".into())),
        Some(t @ Type::Unknown(_)) => {
            return Err(unsupported(format!("physical type {}", t.name())));
        }
        None => return Err(VetaError::user("nested types unsupported")),
    };
    let ptype = physical.parquet_type().name();
    let convert = match (annotation(el)?, physical) {
        (
            Annotation::Decimal { precision, scale },
            Physical::Int32 | Physical::Int64 | Physical::Fixed(_),
        ) => {
            if precision < 1
                || precision > i32::from(MAX_DECIMAL_PRECISION)
                || scale < 0
                || scale > precision
            {
                return Err(unsupported(format!("DECIMAL({precision},{scale})")));
            }
            if let Physical::Fixed(len) = physical
                && len > 16
            {
                return Err(unsupported(format!(
                    "DECIMAL in FIXED_LEN_BYTE_ARRAY({len})"
                )));
            }
            Convert::Decimal {
                precision: precision as u8,
                scale: scale as u8,
            }
        }
        (Annotation::None, Physical::Boolean) => Convert::Boolean,
        (Annotation::None, Physical::Int32) => Convert::Int32,
        (Annotation::None, Physical::Int64) => Convert::Int64,
        (Annotation::None, Physical::Float | Physical::Double) => Convert::Float64,
        (Annotation::None | Annotation::String, Physical::ByteArray) => Convert::Varchar,
        (Annotation::Date, Physical::Int32) => Convert::Date,
        (Annotation::Int { bits, signed }, Physical::Int32) => match (bits, signed) {
            (8, true) => Convert::Int8ToInt16,
            (8, false) => Convert::UInt8ToInt16,
            (16, true) => Convert::Int16,
            (16, false) => Convert::UInt16ToInt32,
            (32, true) => Convert::Int32,
            (32, false) => Convert::UInt32ToInt64,
            _ => return Err(unsupported(format!("INT({bits},{signed}) in INT32"))),
        },
        (Annotation::Int { bits: 64, signed }, Physical::Int64) => {
            if signed {
                Convert::Int64
            } else {
                Convert::UInt64ToInt128
            }
        }
        (Annotation::Timestamp(unit), Physical::Int64) => Convert::Timestamp {
            per_second: match unit {
                TimeUnit::Millis => 1_000,
                TimeUnit::Micros => 1_000_000,
                TimeUnit::Nanos => 1_000_000_000,
            },
        },
        (Annotation::Other(what), _) => return Err(unsupported(format!("{ptype} with {what}"))),
        _ => return Err(unsupported(format!("{ptype} with this annotation"))),
    };
    Ok((physical, convert))
}

/// Flat schema: the root and its leaves, each REQUIRED or OPTIONAL.
fn map_schema(meta: &FileMetaData) -> Result<Vec<ColumnInfo>> {
    let nested = || VetaError::user("nested types unsupported");
    let (root, leaves) = meta
        .schema
        .split_first()
        .ok_or_else(|| invalid("empty schema"))?;
    if root.num_children.unwrap_or(0) as usize != leaves.len() {
        return Err(nested());
    }
    leaves
        .iter()
        .map(|el| {
            if el.num_children.is_some_and(|n| n > 0) || el.type_.is_none() {
                return Err(nested());
            }
            let nullable = match el.repetition_type {
                None | Some(FieldRepetitionType::Required) => false,
                Some(FieldRepetitionType::Optional) => true,
                Some(_) => return Err(nested()),
            };
            let (physical, convert) = map_column(el)?;
            Ok(ColumnInfo {
                name: el.name.clone(),
                physical,
                convert,
                nullable,
            })
        })
        .collect()
}

fn check_codec(codec: CompressionCodec) -> Result<()> {
    match codec {
        CompressionCodec::Uncompressed | CompressionCodec::Snappy => Ok(()),
        other => Err(VetaError::user(format!(
            "unsupported compression {}",
            other.name()
        ))),
    }
}

/// An open Parquet file with a flat schema.
#[derive(Debug)]
pub struct ParquetFile {
    file: File,
    file_len: u64,
    meta: FileMetaData,
    schema: Schema,
    columns: Vec<ColumnInfo>,
}

impl ParquetFile {
    /// Opens `path`, reads its footer and maps the schema. Nested or REPEATED columns,
    /// INT96, unsupported annotations and chunks compressed with anything other than
    /// UNCOMPRESSED or SNAPPY are errors here.
    pub fn open(path: impl AsRef<Path>) -> Result<ParquetFile> {
        let path = path.as_ref();
        let file =
            File::open(path).map_err(|e| VetaError::Io(format!("{}: {e}", path.display())))?;
        let file_len = file.metadata()?.len();
        let meta = read_footer(&file)?;
        let columns = map_schema(&meta)?;
        for (g, rg) in meta.row_groups.iter().enumerate() {
            if rg.num_rows < 0 {
                return Err(invalid(format!("row group {g} has {} rows", rg.num_rows)));
            }
            if rg.columns.len() != columns.len() {
                return Err(invalid(format!(
                    "row group {g} has {} column chunks for {} columns",
                    rg.columns.len(),
                    columns.len()
                )));
            }
            for (chunk, info) in rg.columns.iter().zip(&columns) {
                if chunk.file_path.is_some() {
                    return Err(VetaError::unsupported(
                        "parquet column chunk in another file",
                    ));
                }
                let md = chunk
                    .meta_data
                    .as_ref()
                    .ok_or_else(|| invalid(format!("row group {g} chunk without metadata")))?;
                check_codec(md.codec)?;
                if md.type_ != info.physical.parquet_type() {
                    return Err(invalid(format!(
                        "column {} is {} in the schema and {} in row group {g}",
                        info.name,
                        info.physical.parquet_type().name(),
                        md.type_.name()
                    )));
                }
            }
        }
        let schema = Schema::new(
            columns
                .iter()
                .map(|c| Field::new(c.name.clone(), c.convert.data_type(), c.nullable))
                .collect(),
        );
        Ok(ParquetFile {
            file,
            file_len,
            meta,
            schema,
            columns,
        })
    }

    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    pub fn metadata(&self) -> &FileMetaData {
        &self.meta
    }

    pub fn num_row_groups(&self) -> usize {
        self.meta.row_groups.len()
    }

    pub fn row_group_rows(&self, row_group: usize) -> usize {
        self.meta.row_groups[row_group].num_rows as usize
    }

    pub fn num_rows(&self) -> u64 {
        self.meta
            .row_groups
            .iter()
            .map(|rg| rg.num_rows as u64)
            .sum()
    }

    fn chunk_meta(&self, row_group: usize, column: usize) -> Result<&ColumnMetaData> {
        let rg =
            self.meta.row_groups.get(row_group).ok_or_else(|| {
                VetaError::internal(format!("row group {row_group} out of range"))
            })?;
        let chunk = rg
            .columns
            .get(column)
            .ok_or_else(|| VetaError::internal(format!("column {column} out of range")))?;
        chunk
            .meta_data
            .as_ref()
            .ok_or_else(|| invalid("column chunk without metadata"))
    }

    /// Reads and decodes one column chunk entirely.
    pub fn read_column_chunk(&self, row_group: usize, column: usize) -> Result<Vector> {
        let md = self.chunk_meta(row_group, column)?;
        let info = &self.columns[column];
        let rows = self.row_group_rows(row_group);
        let where_ = || format!("column {} of row group {row_group}", info.name);
        check_codec(md.codec)?;
        if md.num_values != rows as i64 {
            return Err(invalid(format!(
                "{} has {} values for {rows} rows",
                where_(),
                md.num_values
            )));
        }
        let mut start = md.data_page_offset;
        if let Some(dict) = md.dictionary_page_offset
            && dict > 0
            && dict < start
        {
            start = dict;
        }
        let len = md.total_compressed_size;
        let end = start.checked_add(len);
        if start < 4 || len < 0 || end.is_none_or(|e| e as u64 > self.file_len) {
            return Err(invalid(format!(
                "{} at offset {start} with {len} bytes is outside the file",
                where_()
            )));
        }
        let mut buf = vec![0u8; len as usize];
        self.file.read_exact_at(&mut buf, start as u64)?;
        let mut decoder = ChunkDecoder::new(info, md.codec, rows);
        decoder
            .decode_pages(&buf)
            .map_err(|e| context(e, &where_()))?;
        decoder.finish().map_err(|e| context(e, &where_()))
    }

    /// Iterator over `columns` of every row group, in chunks of up to [`CHUNK_ROWS`] rows.
    pub fn chunks(&self, columns: &[usize]) -> Result<ChunkIter<'_>> {
        self.scan((0..self.num_row_groups()).collect(), columns.to_vec())
    }

    /// Iterator over `columns` of the given row groups, in that order. A row group is
    /// decoded whole when its first chunk is needed.
    pub fn scan(&self, row_groups: Vec<usize>, columns: Vec<usize>) -> Result<ChunkIter<'_>> {
        if let Some(&g) = row_groups.iter().find(|&&g| g >= self.num_row_groups()) {
            return Err(VetaError::internal(format!("row group {g} out of range")));
        }
        if let Some(&c) = columns.iter().find(|&&c| c >= self.columns.len()) {
            return Err(VetaError::internal(format!("column {c} out of range")));
        }
        Ok(ChunkIter {
            file: self,
            columns,
            row_groups: row_groups.into_iter(),
            current: Vec::new(),
            offset: 0,
            rows: 0,
        })
    }

    /// `(min, max, null count)` of a column chunk, for row-group skipping (contract §9):
    /// `min_value`/`max_value` and, when both are missing, the legacy `min`/`max` only for
    /// signed integers and dates. `None` when any of the three is unknown or unusable
    /// (a NaN bound, a value of the wrong size).
    pub fn row_group_stats(&self, row_group: usize, column: usize) -> Option<(Value, Value, u64)> {
        let md = self.chunk_meta(row_group, column).ok()?;
        let info = self.columns.get(column)?;
        let stats = md.statistics.as_ref()?;
        let nulls = u64::try_from(stats.null_count?).ok()?;
        let (min, max) = match (&stats.min_value, &stats.max_value) {
            (Some(min), Some(max)) => (min, max),
            (None, None) if info.convert.trusts_legacy_stats() => {
                (stats.min.as_ref()?, stats.max.as_ref()?)
            }
            _ => return None,
        };
        let min = stat_value(info, min)?;
        let max = stat_value(info, max)?;
        Some((min, max, nulls))
    }
}

fn context(e: VetaError, where_: &str) -> VetaError {
    match e {
        VetaError::User(msg) => VetaError::User(format!("{msg} ({where_})")),
        other => other,
    }
}

/// Iterator of [`Chunk`]s returned by [`ParquetFile::chunks`] and [`ParquetFile::scan`].
pub struct ChunkIter<'a> {
    file: &'a ParquetFile,
    columns: Vec<usize>,
    row_groups: std::vec::IntoIter<usize>,
    current: Vec<Vector>,
    offset: usize,
    rows: usize,
}

impl Iterator for ChunkIter<'_> {
    type Item = Result<Chunk>;

    fn next(&mut self) -> Option<Result<Chunk>> {
        loop {
            if self.offset < self.rows {
                let len = CHUNK_ROWS.min(self.rows - self.offset);
                let columns = if self.offset == 0 && len == self.rows {
                    std::mem::take(&mut self.current)
                } else {
                    self.current
                        .iter()
                        .map(|v| v.slice(self.offset, len))
                        .collect()
                };
                self.offset += len;
                return Some(Chunk::with_len(columns, len));
            }
            let g = self.row_groups.next()?;
            let loaded: Result<Vec<Vector>> = self
                .columns
                .iter()
                .map(|&c| self.file.read_column_chunk(g, c))
                .collect();
            match loaded {
                Ok(vectors) => {
                    self.current = vectors;
                    self.offset = 0;
                    self.rows = self.file.row_group_rows(g);
                }
                Err(e) => {
                    self.current.clear();
                    self.offset = 0;
                    self.rows = 0;
                    return Some(Err(e));
                }
            }
        }
    }
}

// ---- statistics ----

fn le<const N: usize>(bytes: &[u8]) -> Option<[u8; N]> {
    bytes.try_into().ok()
}

/// Big-endian two's complement of up to 16 bytes, sign-extended.
fn be_signed(bytes: &[u8]) -> i128 {
    let mut v: i128 = if bytes.first().is_some_and(|b| b & 0x80 != 0) {
        -1
    } else {
        0
    };
    for &b in bytes {
        v = (v << 8) | i128::from(b);
    }
    v
}

fn decimal_value(value: i128, precision: u8, scale: u8) -> Value {
    Value::Decimal {
        value,
        precision,
        scale,
    }
}

/// A statistics bound in the plain encoding of the physical type, as a veta value.
fn stat_value(info: &ColumnInfo, bytes: &[u8]) -> Option<Value> {
    Some(match (info.physical, info.convert) {
        (Physical::Boolean, _) => Value::Boolean(*le::<1>(bytes)?.first()? != 0),
        (Physical::Int32, convert) => {
            let v = i32::from_le_bytes(le(bytes)?);
            match convert {
                Convert::Int8ToInt16 => Value::Int16(v as i8 as i16),
                Convert::UInt8ToInt16 => Value::Int16(v as u8 as i16),
                Convert::Int16 => Value::Int16(v as i16),
                Convert::UInt16ToInt32 => Value::Int32(v as u16 as i32),
                Convert::Int32 => Value::Int32(v),
                Convert::UInt32ToInt64 => Value::Int64(v as u32 as i64),
                Convert::Date => Value::Date(v),
                Convert::Decimal { precision, scale } => {
                    decimal_value(i128::from(v), precision, scale)
                }
                _ => return None,
            }
        }
        (Physical::Int64, convert) => {
            let v = i64::from_le_bytes(le(bytes)?);
            match convert {
                Convert::Int64 => Value::Int64(v),
                Convert::UInt64ToInt128 => Value::Int128(v as u64 as i128),
                Convert::Timestamp { per_second } => Value::Timestamp(v.div_euclid(per_second)),
                Convert::Decimal { precision, scale } => {
                    decimal_value(i128::from(v), precision, scale)
                }
                _ => return None,
            }
        }
        (Physical::Float, _) => {
            let v = f64::from(f32::from_le_bytes(le(bytes)?));
            if v.is_nan() {
                return None;
            }
            Value::Float64(v)
        }
        (Physical::Double, _) => {
            let v = f64::from_le_bytes(le(bytes)?);
            if v.is_nan() {
                return None;
            }
            Value::Float64(v)
        }
        (Physical::ByteArray, _) => Value::Varchar(bytes.to_vec()),
        (Physical::Fixed(width), Convert::Decimal { precision, scale }) => {
            if bytes.is_empty() || bytes.len() > width {
                return None;
            }
            decimal_value(be_signed(bytes), precision, scale)
        }
        (Physical::Fixed(_), _) => return None,
    })
}

// ---- pages ----

/// Physical values of the non-NULL rows read so far.
enum Values {
    Bool(Vec<bool>),
    I32(Vec<i32>),
    I64(Vec<i64>),
    F64(Vec<f64>),
    Bytes { offsets: Vec<u32>, bytes: Vec<u8> },
}

impl Values {
    fn new(physical: Physical, capacity: usize) -> Values {
        match physical {
            Physical::Boolean => Values::Bool(Vec::with_capacity(capacity)),
            Physical::Int32 => Values::I32(Vec::with_capacity(capacity)),
            Physical::Int64 => Values::I64(Vec::with_capacity(capacity)),
            Physical::Float | Physical::Double => Values::F64(Vec::with_capacity(capacity)),
            Physical::ByteArray | Physical::Fixed(_) => {
                let mut offsets = Vec::with_capacity(capacity + 1);
                offsets.push(0);
                Values::Bytes {
                    offsets,
                    bytes: Vec::new(),
                }
            }
        }
    }

    fn len(&self) -> usize {
        match self {
            Values::Bool(v) => v.len(),
            Values::I32(v) => v.len(),
            Values::I64(v) => v.len(),
            Values::F64(v) => v.len(),
            Values::Bytes { offsets, .. } => offsets.len() - 1,
        }
    }

    /// PLAIN decoding of `count` values of `physical`; returns the bytes consumed.
    fn decode_plain(&mut self, physical: Physical, data: &[u8], count: usize) -> Result<usize> {
        match (self, physical) {
            (Values::Bool(v), _) => encodings::plain_bool(data, count, v),
            (Values::I32(v), _) => encodings::plain_i32(data, count, v),
            (Values::I64(v), _) => encodings::plain_i64(data, count, v),
            (Values::F64(v), Physical::Float) => encodings::plain_f32(data, count, v),
            (Values::F64(v), _) => encodings::plain_f64(data, count, v),
            (Values::Bytes { offsets, bytes }, Physical::Fixed(width)) => {
                encodings::plain_fixed_len(data, count, width, &mut ByteArrays { offsets, bytes })
            }
            (Values::Bytes { offsets, bytes }, _) => {
                encodings::plain_byte_array(data, count, &mut ByteArrays { offsets, bytes })
            }
        }
    }

    /// Appends `dict[i]` for every index.
    fn extend_from_dict(&mut self, dict: &Values, indices: &[u32]) -> Result<()> {
        let size = dict.len();
        if let Some(&bad) = indices.iter().find(|&&i| i as usize >= size) {
            return Err(invalid(format!(
                "dictionary index {bad} out of a dictionary of {size}"
            )));
        }
        match (self, dict) {
            (Values::Bool(out), Values::Bool(d)) => {
                out.extend(indices.iter().map(|&i| d[i as usize]))
            }
            (Values::I32(out), Values::I32(d)) => {
                out.extend(indices.iter().map(|&i| d[i as usize]))
            }
            (Values::I64(out), Values::I64(d)) => {
                out.extend(indices.iter().map(|&i| d[i as usize]))
            }
            (Values::F64(out), Values::F64(d)) => {
                out.extend(indices.iter().map(|&i| d[i as usize]))
            }
            (
                Values::Bytes { offsets, bytes },
                Values::Bytes {
                    offsets: d_offsets,
                    bytes: d_bytes,
                },
            ) => {
                offsets.reserve(indices.len());
                for &i in indices {
                    let i = i as usize;
                    bytes.extend_from_slice(
                        &d_bytes[d_offsets[i] as usize..d_offsets[i + 1] as usize],
                    );
                    let end = u32::try_from(bytes.len())
                        .map_err(|_| invalid("byte arrays of a column chunk exceed 4 GiB"))?;
                    offsets.push(end);
                }
            }
            _ => return Err(VetaError::internal("dictionary of another physical type")),
        }
        Ok(())
    }
}

struct ChunkDecoder<'a> {
    info: &'a ColumnInfo,
    codec: CompressionCodec,
    rows: usize,
    /// Levels (rows) read so far.
    levels: usize,
    values: Values,
    /// One entry per row when the column is OPTIONAL.
    valid: Vec<bool>,
    dict: Option<Values>,
}

impl<'a> ChunkDecoder<'a> {
    fn new(info: &'a ColumnInfo, codec: CompressionCodec, rows: usize) -> ChunkDecoder<'a> {
        ChunkDecoder {
            info,
            codec,
            rows,
            levels: 0,
            values: Values::new(info.physical, rows),
            valid: if info.nullable {
                Vec::with_capacity(rows)
            } else {
                Vec::new()
            },
            dict: None,
        }
    }

    fn decompress<'b>(&self, data: &'b [u8], expected: usize) -> Result<Cow<'b, [u8]>> {
        let out = match self.codec {
            CompressionCodec::Uncompressed => Cow::Borrowed(data),
            CompressionCodec::Snappy => Cow::Owned(snappy::decompress(data)?),
            other => {
                return Err(VetaError::user(format!(
                    "unsupported compression {}",
                    other.name()
                )));
            }
        };
        if out.len() != expected {
            return Err(invalid(format!(
                "page decompresses to {} bytes, header says {expected}",
                out.len()
            )));
        }
        Ok(out)
    }

    fn decode_pages(&mut self, buf: &[u8]) -> Result<()> {
        let mut pos = 0;
        while self.levels < self.rows {
            if pos >= buf.len() {
                return Err(invalid(format!(
                    "column chunk ends after {} of {} rows",
                    self.levels, self.rows
                )));
            }
            let (header, used) = decode::<PageHeader>(&buf[pos..])?;
            pos += used;
            let size = usize::try_from(header.compressed_page_size)
                .map_err(|_| invalid("negative compressed page size"))?;
            let uncompressed = usize::try_from(header.uncompressed_page_size)
                .map_err(|_| invalid("negative uncompressed page size"))?;
            if size > buf.len() - pos {
                return Err(invalid(format!(
                    "page of {size} bytes past the end of the column chunk"
                )));
            }
            let body = &buf[pos..pos + size];
            pos += size;
            if let Some(crc) = header.crc
                && crc32(body) != crc as u32
            {
                return Err(invalid(format!(
                    "page crc mismatch: header {:08x}, data {:08x}",
                    crc as u32,
                    crc32(body)
                )));
            }
            match header.type_ {
                PageType::DictionaryPage => self.dictionary_page(&header, body, uncompressed)?,
                PageType::DataPage => self.data_page_v1(&header, body, uncompressed)?,
                PageType::DataPageV2 => self.data_page_v2(&header, body, uncompressed)?,
                _ => {}
            }
        }
        Ok(())
    }

    fn dictionary_page(
        &mut self,
        header: &PageHeader,
        body: &[u8],
        uncompressed: usize,
    ) -> Result<()> {
        let h = header
            .dictionary_page_header
            .as_ref()
            .ok_or_else(|| invalid("DICTIONARY_PAGE without its header"))?;
        if !matches!(h.encoding, Encoding::Plain | Encoding::PlainDictionary) {
            return Err(VetaError::unsupported(format!(
                "parquet dictionary page encoding {}",
                h.encoding.name()
            )));
        }
        let count =
            usize::try_from(h.num_values).map_err(|_| invalid("negative dictionary size"))?;
        let data = self.decompress(body, uncompressed)?;
        let mut dict = Values::new(self.info.physical, count.min(data.len()));
        dict.decode_plain(self.info.physical, &data, count)?;
        self.dict = Some(dict);
        Ok(())
    }

    /// Takes `count` levels; returns how many rows are non-NULL.
    fn take_levels(&mut self, count: usize) -> Result<usize> {
        if count > self.rows - self.levels {
            return Err(invalid(format!(
                "pages hold more than the {} rows of the chunk",
                self.rows
            )));
        }
        self.levels += count;
        Ok(count)
    }

    fn page_count(n: i32) -> Result<usize> {
        usize::try_from(n).map_err(|_| invalid("negative page value count"))
    }

    fn data_page_v1(
        &mut self,
        header: &PageHeader,
        body: &[u8],
        uncompressed: usize,
    ) -> Result<()> {
        let h = header
            .data_page_header
            .as_ref()
            .ok_or_else(|| invalid("DATA_PAGE without its header"))?;
        let n = self.take_levels(Self::page_count(h.num_values)?)?;
        let data = self.decompress(body, uncompressed)?;
        let mut pos = 0;
        let present = if self.info.nullable {
            match h.definition_level_encoding {
                Encoding::Rle => {}
                Encoding::BitPacked => {
                    return Err(VetaError::unsupported("parquet encoding BIT_PACKED"));
                }
                other => {
                    return Err(invalid(format!("definition levels in {}", other.name())));
                }
            }
            let before = self.valid.len();
            pos = encodings::decode_def_levels(&data, n, true, &mut self.valid)?;
            self.valid[before..].iter().filter(|&&v| v).count()
        } else {
            n
        };
        self.decode_values(h.encoding, &data[pos..], present)
    }

    fn data_page_v2(
        &mut self,
        header: &PageHeader,
        body: &[u8],
        uncompressed: usize,
    ) -> Result<()> {
        let h = header
            .data_page_header_v2
            .as_ref()
            .ok_or_else(|| invalid("DATA_PAGE_V2 without its header"))?;
        let n = self.take_levels(Self::page_count(h.num_values)?)?;
        let rep = usize::try_from(h.repetition_levels_byte_length)
            .map_err(|_| invalid("negative repetition levels length"))?;
        let def = usize::try_from(h.definition_levels_byte_length)
            .map_err(|_| invalid("negative definition levels length"))?;
        if rep + def > body.len() || rep + def > uncompressed {
            return Err(invalid("levels longer than the page"));
        }
        let present = if self.info.nullable {
            let before = self.valid.len();
            encodings::decode_def_levels(&body[rep..rep + def], n, false, &mut self.valid)?;
            let present = self.valid[before..].iter().filter(|&&v| v).count();
            if usize::try_from(h.num_nulls).ok() != Some(n - present) {
                return Err(invalid(format!(
                    "DATA_PAGE_V2 declares {} nulls, levels have {}",
                    h.num_nulls,
                    n - present
                )));
            }
            present
        } else {
            n
        };
        let values = &body[rep + def..];
        let expected = uncompressed - rep - def;
        let data = if h.is_compressed() {
            self.decompress(values, expected)?
        } else {
            if values.len() != expected {
                return Err(invalid("uncompressed page size does not match its data"));
            }
            Cow::Borrowed(values)
        };
        self.decode_values(h.encoding, &data, present)
    }

    /// Decodes `count` non-NULL values in `encoding` from `data`.
    fn decode_values(&mut self, encoding: Encoding, data: &[u8], count: usize) -> Result<()> {
        let physical = self.info.physical;
        let before = self.values.len();
        let bad = |what: &str| {
            VetaError::unsupported(format!(
                "parquet encoding {what} for {}",
                physical.parquet_type().name()
            ))
        };
        match encoding {
            Encoding::Plain => {
                self.values.decode_plain(physical, data, count)?;
            }
            Encoding::PlainDictionary | Encoding::RleDictionary => {
                let dict = self
                    .dict
                    .as_ref()
                    .ok_or_else(|| invalid("dictionary-encoded page without a dictionary page"))?;
                let mut indices = Vec::with_capacity(count);
                encodings::decode_dict_indices(data, count, &mut indices)?;
                self.values.extend_from_dict(dict, &indices)?;
            }
            Encoding::Rle => match &mut self.values {
                Values::Bool(v) => {
                    encodings::decode_rle_booleans(data, count, v)?;
                }
                _ => return Err(bad("RLE")),
            },
            Encoding::DeltaBinaryPacked => match &mut self.values {
                Values::I64(v) => {
                    encodings::decode_delta_binary_packed(data, v)?;
                }
                Values::I32(v) => {
                    let mut wide = Vec::with_capacity(count);
                    encodings::decode_delta_binary_packed(data, &mut wide)?;
                    v.extend(wide.iter().map(|&x| x as i32));
                }
                _ => return Err(bad("DELTA_BINARY_PACKED")),
            },
            Encoding::DeltaLengthByteArray => match (&mut self.values, physical) {
                (Values::Bytes { offsets, bytes }, Physical::ByteArray) => {
                    encodings::decode_delta_length_byte_array(
                        data,
                        &mut ByteArrays { offsets, bytes },
                    )?;
                }
                _ => return Err(bad("DELTA_LENGTH_BYTE_ARRAY")),
            },
            Encoding::DeltaByteArray => match &mut self.values {
                Values::Bytes { offsets, bytes } => {
                    encodings::decode_delta_byte_array(
                        data,
                        &mut ByteArrays {
                            offsets: &mut *offsets,
                            bytes: &mut *bytes,
                        },
                    )?;
                    if let Physical::Fixed(width) = physical
                        && offsets[before..]
                            .windows(2)
                            .any(|w| (w[1] - w[0]) as usize != width)
                    {
                        return Err(invalid(format!(
                            "DELTA_BYTE_ARRAY value is not {width} bytes long"
                        )));
                    }
                }
                _ => return Err(bad("DELTA_BYTE_ARRAY")),
            },
            Encoding::ByteStreamSplit => match (&mut self.values, physical) {
                (Values::I32(v), _) => {
                    v.reserve(count);
                    encodings::byte_stream_split::<4>(data, count, |b| {
                        v.push(i32::from_le_bytes(b))
                    })?;
                }
                (Values::I64(v), _) => {
                    v.reserve(count);
                    encodings::byte_stream_split::<8>(data, count, |b| {
                        v.push(i64::from_le_bytes(b))
                    })?;
                }
                (Values::F64(v), Physical::Float) => {
                    v.reserve(count);
                    encodings::byte_stream_split::<4>(data, count, |b| {
                        v.push(f64::from(f32::from_le_bytes(b)))
                    })?;
                }
                (Values::F64(v), _) => {
                    v.reserve(count);
                    encodings::byte_stream_split::<8>(data, count, |b| {
                        v.push(f64::from_le_bytes(b))
                    })?;
                }
                (Values::Bytes { offsets, bytes }, Physical::Fixed(width)) => {
                    encodings::byte_stream_split_fixed(
                        data,
                        count,
                        width,
                        &mut ByteArrays { offsets, bytes },
                    )?;
                }
                _ => return Err(bad("BYTE_STREAM_SPLIT")),
            },
            Encoding::BitPacked => {
                return Err(VetaError::unsupported("parquet encoding BIT_PACKED"));
            }
            other => return Err(bad(&other.name())),
        }
        let got = self.values.len() - before;
        if got != count {
            return Err(invalid(format!(
                "page has {got} values in {}, levels say {count}",
                encoding.name()
            )));
        }
        Ok(())
    }

    /// Converts the dense non-NULL values to the veta type and spreads them over the rows.
    fn finish(self) -> Result<Vector> {
        if self.levels != self.rows {
            return Err(invalid(format!(
                "pages hold {} of {} rows",
                self.levels, self.rows
            )));
        }
        let valid = if self.info.nullable && self.valid.iter().any(|&v| !v) {
            Some(self.valid.as_slice())
        } else {
            None
        };
        let data = match (self.info.convert, self.values) {
            (Convert::Boolean, Values::Bool(v)) => VectorData::Boolean(expand(&v, valid, |x| x)),
            (Convert::Int8ToInt16, Values::I32(v)) => {
                VectorData::Int16(expand(&v, valid, |x| x as i8 as i16))
            }
            (Convert::UInt8ToInt16, Values::I32(v)) => {
                VectorData::Int16(expand(&v, valid, |x| x as u8 as i16))
            }
            (Convert::Int16, Values::I32(v)) => VectorData::Int16(expand(&v, valid, |x| x as i16)),
            (Convert::UInt16ToInt32, Values::I32(v)) => {
                VectorData::Int32(expand(&v, valid, |x| x as u16 as i32))
            }
            (Convert::Int32, Values::I32(v)) => VectorData::Int32(expand_owned(v, valid)),
            (Convert::UInt32ToInt64, Values::I32(v)) => {
                VectorData::Int64(expand(&v, valid, |x| x as u32 as i64))
            }
            (Convert::Date, Values::I32(v)) => VectorData::Date(expand_owned(v, valid)),
            (Convert::Int64, Values::I64(v)) => VectorData::Int64(expand_owned(v, valid)),
            (Convert::UInt64ToInt128, Values::I64(v)) => {
                VectorData::Int128(expand(&v, valid, |x| x as u64 as i128))
            }
            (Convert::Timestamp { per_second }, Values::I64(v)) => {
                VectorData::Timestamp(expand(&v, valid, |x| x.div_euclid(per_second)))
            }
            (Convert::Decimal { precision, scale }, values) => {
                decimal_data(values, precision, scale, valid)?
            }
            (Convert::Float64, Values::F64(v)) => VectorData::Float64(expand_owned(v, valid)),
            (Convert::Varchar, Values::Bytes { offsets, bytes }) => VectorData::Varchar {
                offsets: match valid {
                    None => offsets,
                    Some(valid) => expand_offsets(&offsets, valid),
                },
                bytes,
            },
            _ => {
                return Err(VetaError::internal(
                    "column conversion does not match its values",
                ));
            }
        };
        let validity = valid.map(Bitmap::from_bools);
        Vector::new(data, validity)
    }
}

fn decimal_data(
    values: Values,
    precision: u8,
    scale: u8,
    valid: Option<&[bool]>,
) -> Result<VectorData> {
    let wide: Vec<i128> = match values {
        Values::I32(v) => expand(&v, valid, i128::from),
        Values::I64(v) => expand(&v, valid, i128::from),
        Values::Bytes { offsets, bytes } => {
            let dense: Vec<i128> = offsets
                .windows(2)
                .map(|w| be_signed(&bytes[w[0] as usize..w[1] as usize]))
                .collect();
            expand_owned(dense, valid)
        }
        _ => {
            return Err(VetaError::internal(
                "decimal over a non-integer physical type",
            ));
        }
    };
    Ok(if precision <= MAX_DECIMAL64_PRECISION {
        VectorData::Decimal64 {
            values: wide.into_iter().map(|v| v as i64).collect(),
            precision,
            scale,
        }
    } else {
        VectorData::Decimal128 {
            values: wide,
            precision,
            scale,
        }
    })
}

/// Maps the dense values and places them on the valid rows; NULL rows get the default.
fn expand<S: Copy, T: Copy + Default>(
    dense: &[S],
    valid: Option<&[bool]>,
    f: impl Fn(S) -> T,
) -> Vec<T> {
    match valid {
        None => dense.iter().map(|&x| f(x)).collect(),
        Some(valid) => {
            let mut out = Vec::with_capacity(valid.len());
            let mut it = dense.iter();
            for &v in valid {
                out.push(if v {
                    it.next().map_or_else(T::default, |&x| f(x))
                } else {
                    T::default()
                });
            }
            out
        }
    }
}

/// [`expand`] without conversion, reusing the buffer when there are no NULLs.
fn expand_owned<T: Copy + Default>(dense: Vec<T>, valid: Option<&[bool]>) -> Vec<T> {
    match valid {
        None => dense,
        Some(_) => expand(&dense, valid, |x| x),
    }
}

/// Offsets of the dense strings spread over the rows: a NULL row is an empty string.
fn expand_offsets(dense: &[u32], valid: &[bool]) -> Vec<u32> {
    let mut out = Vec::with_capacity(valid.len() + 1);
    out.push(0);
    let mut next = 1;
    let mut last = 0;
    for &v in valid {
        if v && next < dense.len() {
            last = dense[next];
            next += 1;
        }
        out.push(last);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::footer::{MAGIC, write_footer};
    use crate::meta::{
        ColumnChunk, DataPageHeader, DataPageHeaderV2, DictionaryPageHeader, RowGroup, Statistics,
        encode,
    };
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct TempFile(PathBuf);

    impl TempFile {
        fn with(bytes: &[u8]) -> TempFile {
            static N: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "veta-reader-{}-{}.parquet",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::write(&path, bytes).unwrap();
            TempFile(path)
        }

        fn open(&self) -> Result<ParquetFile> {
            ParquetFile::open(&self.0)
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn uleb(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let b = (v & 0x7F) as u8;
            v >>= 7;
            if v == 0 {
                out.push(b);
                return out;
            }
            out.push(b | 0x80);
        }
    }

    fn zz(v: i64) -> Vec<u8> {
        uleb(((v << 1) ^ (v >> 63)) as u64)
    }

    fn leaf(
        name: &str,
        type_: Type,
        repetition: FieldRepetitionType,
        logical_type: Option<LogicalType>,
    ) -> SchemaElement {
        SchemaElement {
            type_: Some(type_),
            type_length: None,
            repetition_type: Some(repetition),
            name: name.into(),
            num_children: None,
            converted_type: None,
            scale: None,
            precision: None,
            field_id: None,
            logical_type,
        }
    }

    /// One column of the test file: its schema element and its pages, the first of which
    /// is the dictionary page when `dict_page_len` is set.
    struct Column {
        element: SchemaElement,
        pages: Vec<u8>,
        dict_page_len: Option<usize>,
        codec: CompressionCodec,
        encodings: Vec<Encoding>,
        statistics: Option<Statistics>,
    }

    fn page(header: PageHeader, body: &[u8]) -> Vec<u8> {
        let mut out = encode(&header);
        out.extend_from_slice(body);
        out
    }

    fn build(columns: Vec<Column>, rows: i64) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        let mut schema = vec![SchemaElement {
            type_: None,
            type_length: None,
            repetition_type: None,
            name: "schema".into(),
            num_children: Some(columns.len() as i32),
            converted_type: None,
            scale: None,
            precision: None,
            field_id: None,
            logical_type: None,
        }];
        let mut chunks = Vec::new();
        for c in columns {
            let start = out.len() as i64;
            out.extend_from_slice(&c.pages);
            let data_page_offset = start + c.dict_page_len.unwrap_or(0) as i64;
            chunks.push(ColumnChunk {
                file_path: None,
                file_offset: start,
                meta_data: Some(ColumnMetaData {
                    type_: c.element.type_.unwrap(),
                    encodings: c.encodings,
                    path_in_schema: vec![c.element.name.clone()],
                    codec: c.codec,
                    num_values: rows,
                    total_uncompressed_size: c.pages.len() as i64,
                    total_compressed_size: c.pages.len() as i64,
                    key_value_metadata: None,
                    data_page_offset,
                    index_page_offset: None,
                    dictionary_page_offset: c.dict_page_len.map(|_| start),
                    statistics: c.statistics,
                }),
            });
            schema.push(c.element);
        }
        let meta = FileMetaData {
            version: 2,
            schema,
            num_rows: rows,
            row_groups: vec![RowGroup {
                columns: chunks,
                total_byte_size: out.len() as i64,
                num_rows: rows,
                file_offset: Some(4),
                total_compressed_size: None,
                ordinal: Some(0),
            }],
            key_value_metadata: None,
            created_by: Some("veta test".into()),
            column_orders: None,
        };
        write_footer(&mut out, &meta).unwrap();
        out
    }

    /// `a`: OPTIONAL INT32 [1, NULL, 3, NULL, 5] in one v1 PLAIN page with definition
    /// levels, Snappy, with a CRC (`crc_delta` corrupts it) and statistics.
    fn column_a(codec: CompressionCodec, crc_delta: u32) -> Column {
        let mut raw = 2u32.to_le_bytes().to_vec();
        raw.extend_from_slice(&[(1 << 1) | 1, 0b0001_0101]); // levels 1,0,1,0,1
        for v in [1i32, 3, 5] {
            raw.extend_from_slice(&v.to_le_bytes());
        }
        let stored = snappy::compress(&raw);
        let header = PageHeader {
            type_: PageType::DataPage,
            uncompressed_page_size: raw.len() as i32,
            compressed_page_size: stored.len() as i32,
            crc: Some(crc32(&stored).wrapping_add(crc_delta) as i32),
            data_page_header: Some(DataPageHeader {
                num_values: 5,
                encoding: Encoding::Plain,
                definition_level_encoding: Encoding::Rle,
                repetition_level_encoding: Encoding::Rle,
                statistics: None,
            }),
            dictionary_page_header: None,
            data_page_header_v2: None,
        };
        Column {
            element: leaf("a", Type::Int32, FieldRepetitionType::Optional, None),
            pages: page(header, &stored),
            dict_page_len: None,
            codec,
            encodings: vec![Encoding::Plain, Encoding::Rle],
            statistics: Some(Statistics {
                min_value: Some(1i32.to_le_bytes().to_vec()),
                max_value: Some(5i32.to_le_bytes().to_vec()),
                null_count: Some(2),
                ..Statistics::default()
            }),
        }
    }

    /// `s`: OPTIONAL STRING ["x", NULL, "yy", "yy", "x"]: a PLAIN dictionary page and a v2
    /// RLE_DICTIONARY page, both Snappy (the v2 levels stay uncompressed).
    fn column_s() -> Column {
        let mut dict = Vec::new();
        for s in [b"x".as_slice(), b"yy"] {
            dict.extend_from_slice(&(s.len() as u32).to_le_bytes());
            dict.extend_from_slice(s);
        }
        let dict_stored = snappy::compress(&dict);
        let dict_page = page(
            PageHeader {
                type_: PageType::DictionaryPage,
                uncompressed_page_size: dict.len() as i32,
                compressed_page_size: dict_stored.len() as i32,
                crc: Some(crc32(&dict_stored) as i32),
                data_page_header: None,
                dictionary_page_header: Some(DictionaryPageHeader {
                    num_values: 2,
                    encoding: Encoding::Plain,
                    is_sorted: None,
                }),
                data_page_header_v2: None,
            },
            &dict_stored,
        );
        let levels = [(1 << 1) | 1, 0b0001_1101]; // 1,0,1,1,1
        let indices = [1, (1 << 1) | 1, 0b0000_0110]; // width 1: 0,1,1,0
        let stored = snappy::compress(&indices);
        let mut body = levels.to_vec();
        body.extend_from_slice(&stored);
        let data_page = page(
            PageHeader {
                type_: PageType::DataPageV2,
                uncompressed_page_size: (levels.len() + indices.len()) as i32,
                compressed_page_size: body.len() as i32,
                crc: None,
                data_page_header: None,
                dictionary_page_header: None,
                data_page_header_v2: Some(DataPageHeaderV2 {
                    num_values: 5,
                    num_nulls: 1,
                    num_rows: 5,
                    encoding: Encoding::RleDictionary,
                    definition_levels_byte_length: levels.len() as i32,
                    repetition_levels_byte_length: 0,
                    is_compressed: Some(true),
                    statistics: None,
                }),
            },
            &body,
        );
        let mut pages = dict_page.clone();
        pages.extend_from_slice(&data_page);
        Column {
            element: leaf(
                "s",
                Type::ByteArray,
                FieldRepetitionType::Optional,
                Some(LogicalType::String),
            ),
            pages,
            dict_page_len: Some(dict_page.len()),
            codec: CompressionCodec::Snappy,
            encodings: vec![Encoding::Plain, Encoding::RleDictionary, Encoding::Rle],
            statistics: None,
        }
    }

    /// `t`: REQUIRED INT64 TIMESTAMP(MICROS) [−1 µs, 1.5 s, 3 s, 3 s, 2.9 s] in one
    /// uncompressed v1 DELTA_BINARY_PACKED page; seconds are floored.
    fn column_t() -> Column {
        let micros = [-1i64, 1_500_000, 3_000_000, 3_000_000, 2_900_000];
        let deltas: Vec<i64> = micros.windows(2).map(|w| w[1] - w[0]).collect();
        let min = *deltas.iter().min().unwrap();
        let mut raw = uleb(128);
        raw.extend(uleb(4));
        raw.extend(uleb(micros.len() as u64));
        raw.extend(zz(micros[0]));
        raw.extend(zz(min));
        // 4 relative deltas ≤ 1_600_001 need 21 bits; first mini-block only, 32 values.
        let width = 21u32;
        raw.extend_from_slice(&[width as u8, 0, 0, 0]);
        let mut packed = vec![0u8; 32 * width as usize / 8];
        for (i, d) in deltas.iter().enumerate() {
            let rel = (d - min) as u64;
            assert!(rel < 1 << width);
            for b in 0..width as usize {
                if rel >> b & 1 == 1 {
                    let bit = i * width as usize + b;
                    packed[bit / 8] |= 1 << (bit % 8);
                }
            }
        }
        raw.extend_from_slice(&packed);
        let header = PageHeader {
            type_: PageType::DataPage,
            uncompressed_page_size: raw.len() as i32,
            compressed_page_size: raw.len() as i32,
            crc: None,
            data_page_header: Some(DataPageHeader {
                num_values: 5,
                encoding: Encoding::DeltaBinaryPacked,
                definition_level_encoding: Encoding::Rle,
                repetition_level_encoding: Encoding::Rle,
                statistics: None,
            }),
            dictionary_page_header: None,
            data_page_header_v2: None,
        };
        Column {
            element: leaf(
                "t",
                Type::Int64,
                FieldRepetitionType::Required,
                Some(LogicalType::Timestamp {
                    is_adjusted_to_utc: false,
                    unit: TimeUnit::Micros,
                }),
            ),
            pages: page(header, &raw),
            dict_page_len: None,
            codec: CompressionCodec::Uncompressed,
            encodings: vec![Encoding::DeltaBinaryPacked],
            statistics: Some(Statistics {
                min: Some((-1i64).to_le_bytes().to_vec()),
                max: Some(3_000_000i64.to_le_bytes().to_vec()),
                null_count: Some(0),
                ..Statistics::default()
            }),
        }
    }

    fn sample_file() -> TempFile {
        TempFile::with(&build(
            vec![
                column_a(CompressionCodec::Snappy, 0),
                column_s(),
                column_t(),
            ],
            5,
        ))
    }

    #[test]
    fn schema_maps_types() {
        let f = sample_file().open().unwrap();
        assert_eq!(
            f.schema().fields,
            vec![
                Field::new("a", DataType::Int32, true),
                Field::new("s", DataType::Varchar, true),
                Field::new("t", DataType::Timestamp, false),
            ]
        );
        assert_eq!(f.num_row_groups(), 1);
        assert_eq!(f.num_rows(), 5);
    }

    #[test]
    fn reads_v1_plain_with_nulls() {
        let file = sample_file();
        let f = file.open().unwrap();
        let a = f.read_column_chunk(0, 0).unwrap();
        let expected = [
            Value::Int32(1),
            Value::Null,
            Value::Int32(3),
            Value::Null,
            Value::Int32(5),
        ];
        assert_eq!(a, Vector::from_values(DataType::Int32, &expected).unwrap());
    }

    #[test]
    fn reads_v2_dictionary_with_nulls() {
        let file = sample_file();
        let f = file.open().unwrap();
        let s = f.read_column_chunk(0, 1).unwrap();
        let expected = [
            Value::varchar("x"),
            Value::Null,
            Value::varchar("yy"),
            Value::varchar("yy"),
            Value::varchar("x"),
        ];
        assert_eq!(
            s,
            Vector::from_values(DataType::Varchar, &expected).unwrap()
        );
    }

    #[test]
    fn reads_delta_timestamps_floored_to_seconds() {
        let file = sample_file();
        let f = file.open().unwrap();
        let t = f.read_column_chunk(0, 2).unwrap();
        assert_eq!(t.validity, None);
        assert_eq!(t.data, VectorData::Timestamp(vec![-1, 1, 3, 3, 2]));
    }

    #[test]
    fn chunk_iterator_returns_the_whole_file() {
        let file = sample_file();
        let f = file.open().unwrap();
        let chunks: Vec<Chunk> = f.chunks(&[1, 0]).unwrap().collect::<Result<_>>().unwrap();
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].len, 5);
        assert_eq!(chunks[0].columns[0].get(2), Value::varchar("yy"));
        assert_eq!(chunks[0].columns[1].get(1), Value::Null);
        assert_eq!(chunks[0].columns[1].get(4), Value::Int32(5));
        let empty: Vec<Chunk> = f.chunks(&[]).unwrap().collect::<Result<_>>().unwrap();
        assert_eq!(empty[0].len, 5);
        assert!(f.scan(vec![1], vec![0]).is_err());
    }

    #[test]
    fn row_group_statistics() {
        let file = sample_file();
        let f = file.open().unwrap();
        assert_eq!(
            f.row_group_stats(0, 0),
            Some((Value::Int32(1), Value::Int32(5), 2))
        );
        assert_eq!(f.row_group_stats(0, 1), None);
        // Legacy min/max are not trusted for timestamps.
        assert_eq!(f.row_group_stats(0, 2), None);
    }

    #[test]
    fn legacy_statistics_for_signed_integers() {
        let mut a = column_a(CompressionCodec::Snappy, 0);
        a.statistics = Some(Statistics {
            min: Some((-4i32).to_le_bytes().to_vec()),
            max: Some(9i32.to_le_bytes().to_vec()),
            null_count: Some(2),
            ..Statistics::default()
        });
        let file = TempFile::with(&build(vec![a], 5));
        let f = file.open().unwrap();
        assert_eq!(
            f.row_group_stats(0, 0),
            Some((Value::Int32(-4), Value::Int32(9), 2))
        );
    }

    #[test]
    fn chunks_of_8192_rows() {
        let rows = 10_000usize;
        let mut raw = Vec::new();
        for v in 0..rows as i32 {
            raw.extend_from_slice(&v.to_le_bytes());
        }
        let header = PageHeader {
            type_: PageType::DataPage,
            uncompressed_page_size: raw.len() as i32,
            compressed_page_size: raw.len() as i32,
            crc: Some(crc32(&raw) as i32),
            data_page_header: Some(DataPageHeader {
                num_values: rows as i32,
                encoding: Encoding::Plain,
                definition_level_encoding: Encoding::Rle,
                repetition_level_encoding: Encoding::BitPacked,
                statistics: None,
            }),
            dictionary_page_header: None,
            data_page_header_v2: None,
        };
        let column = Column {
            element: leaf("n", Type::Int32, FieldRepetitionType::Required, None),
            pages: page(header, &raw),
            dict_page_len: None,
            codec: CompressionCodec::Uncompressed,
            // Old writers list BIT_PACKED for the empty repetition levels; that is fine.
            encodings: vec![Encoding::Plain, Encoding::BitPacked],
            statistics: None,
        };
        let file = TempFile::with(&build(vec![column], rows as i64));
        let f = file.open().unwrap();
        let chunks: Vec<Chunk> = f.chunks(&[0]).unwrap().collect::<Result<_>>().unwrap();
        assert_eq!(
            chunks.iter().map(|c| c.len).collect::<Vec<_>>(),
            [8192, 1808]
        );
        assert_eq!(chunks[1].columns[0].get(0), Value::Int32(8192));
        assert_eq!(chunks[1].columns[0].get(1807), Value::Int32(9999));
    }

    #[test]
    fn zstd_is_rejected() {
        let file = TempFile::with(&build(vec![column_a(CompressionCodec::Zstd, 0)], 5));
        let err = file.open().unwrap_err();
        assert_eq!(err.to_string(), "unsupported compression ZSTD");
    }

    #[test]
    fn repeated_column_is_rejected() {
        let mut a = column_a(CompressionCodec::Snappy, 0);
        a.element.repetition_type = Some(FieldRepetitionType::Repeated);
        let err = TempFile::with(&build(vec![a], 5)).open().unwrap_err();
        assert_eq!(err.to_string(), "nested types unsupported");
    }

    #[test]
    fn group_column_is_rejected() {
        let mut bytes = build(vec![column_a(CompressionCodec::Snappy, 0)], 5);
        // Re-encode the footer with a group in the schema.
        let file = TempFile::with(&bytes);
        let mut meta = file.open().unwrap().metadata().clone();
        meta.schema[1].type_ = None;
        meta.schema[1].num_children = Some(1);
        meta.schema
            .push(leaf("x", Type::Int32, FieldRepetitionType::Required, None));
        let data_end = bytes.len()
            - 8
            - u32::from_le_bytes(bytes[bytes.len() - 8..bytes.len() - 4].try_into().unwrap())
                as usize;
        bytes.truncate(data_end);
        write_footer(&mut bytes, &meta).unwrap();
        let err = TempFile::with(&bytes).open().unwrap_err();
        assert_eq!(err.to_string(), "nested types unsupported");
    }

    #[test]
    fn int96_is_rejected() {
        let mut a = column_a(CompressionCodec::Snappy, 0);
        a.element.type_ = Some(Type::Int96);
        assert!(TempFile::with(&build(vec![a], 5)).open().is_err());
    }

    #[test]
    fn bad_crc_is_an_error() {
        let file = TempFile::with(&build(vec![column_a(CompressionCodec::Snappy, 1)], 5));
        let f = file.open().unwrap();
        let err = f.read_column_chunk(0, 0).unwrap_err();
        assert!(err.to_string().contains("crc"), "{err}");
        let mut it = f.chunks(&[0]).unwrap();
        assert!(it.next().unwrap().is_err());
        assert!(it.next().is_none());
    }

    #[test]
    fn type_mapping() {
        let int = |bits, signed| {
            Some(LogicalType::Integer {
                bit_width: bits,
                is_signed: signed,
            })
        };
        let req = FieldRepetitionType::Required;
        let cases = [
            (leaf("c", Type::Int32, req, int(8, true)), DataType::Int16),
            (leaf("c", Type::Int32, req, int(8, false)), DataType::Int16),
            (leaf("c", Type::Int32, req, int(16, false)), DataType::Int32),
            (leaf("c", Type::Int32, req, int(32, false)), DataType::Int64),
            (
                leaf("c", Type::Int64, req, int(64, false)),
                DataType::Int128,
            ),
            (
                leaf("c", Type::Int32, req, Some(LogicalType::Date)),
                DataType::Date,
            ),
            (leaf("c", Type::Float, req, None), DataType::Float64),
            (leaf("c", Type::Boolean, req, None), DataType::Boolean),
            (
                leaf("c", Type::ByteArray, req, Some(LogicalType::Enum)),
                DataType::Varchar,
            ),
            (
                leaf(
                    "c",
                    Type::Int64,
                    req,
                    Some(LogicalType::Decimal {
                        scale: 2,
                        precision: 15,
                    }),
                ),
                DataType::Decimal {
                    precision: 15,
                    scale: 2,
                },
            ),
        ];
        for (el, expected) in cases {
            assert_eq!(map_column(&el).unwrap().1.data_type(), expected);
        }
        let mut fixed = leaf(
            "c",
            Type::FixedLenByteArray,
            req,
            Some(LogicalType::Decimal {
                scale: 0,
                precision: 30,
            }),
        );
        fixed.type_length = Some(13);
        assert_eq!(
            map_column(&fixed).unwrap(),
            (
                Physical::Fixed(13),
                Convert::Decimal {
                    precision: 30,
                    scale: 0
                }
            )
        );
        let mut legacy = leaf("c", Type::Int64, req, None);
        legacy.converted_type = Some(ConvertedType::TimestampMillis);
        assert_eq!(
            map_column(&legacy).unwrap().1,
            Convert::Timestamp { per_second: 1000 }
        );
        assert!(map_column(&leaf("c", Type::Int96, req, None)).is_err());
        assert!(map_column(&leaf("c", Type::FixedLenByteArray, req, None)).is_err());
        let mut json = leaf("c", Type::ByteArray, req, None);
        json.converted_type = Some(ConvertedType::Json);
        assert!(map_column(&json).is_err());
    }

    #[test]
    fn finish_converts_and_spreads() {
        let info = ColumnInfo {
            name: "d".into(),
            physical: Physical::Fixed(2),
            convert: Convert::Decimal {
                precision: 4,
                scale: 1,
            },
            nullable: true,
        };
        let mut d = ChunkDecoder::new(&info, CompressionCodec::Uncompressed, 3);
        d.levels = 3;
        d.valid = vec![true, false, true];
        d.values = Values::Bytes {
            offsets: vec![0, 2, 4],
            bytes: vec![0xFF, 0x85, 0x01, 0x00],
        };
        let v = d.finish().unwrap();
        assert_eq!(
            v.get(0),
            Value::Decimal {
                value: -123,
                precision: 4,
                scale: 1
            }
        );
        assert_eq!(v.get(1), Value::Null);
        assert_eq!(
            v.get(2),
            Value::Decimal {
                value: 256,
                precision: 4,
                scale: 1
            }
        );
        assert_eq!(
            expand_offsets(&[0, 1, 3], &[false, true, false, true]),
            [0, 0, 1, 1, 3]
        );
        assert_eq!(be_signed(&[0x80, 0]), -32768);
    }
}
