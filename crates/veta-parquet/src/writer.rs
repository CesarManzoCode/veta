//! Parquet writer for flat schemas (contract §9): chunks go in, row groups of up to
//! `row_group_rows` rows go out as soon as they are complete, and the footer closes the file.
//!
//! A column chunk is an optional dictionary page followed by data pages of up to
//! [`PAGE_ROWS`] rows, v1 (levels with a 4-byte length prefix, inside the compression) or v2
//! (levels outside the compression). A flat column has no repetition levels. Statistics are
//! exact over the non-NULL values of the chunk, in the order of the physical type. A chunk
//! with a NaN has no `min_value`/`max_value`: the format forbids a NaN bound (`parquet.thrift`,
//! `ColumnOrder`) and any other bound would exclude rows that the order of contract §4 puts
//! above every number. Zeros keep their sign, so the bounds are the values of the chunk.

use std::collections::HashMap;
use std::hash::Hash;
use std::io::Write;
use std::ops::Range;

use veta_codec::snappy;
use veta_core::types::MAX_DECIMAL64_PRECISION;
use veta_core::{Chunk, DataType, Field, Result, Schema, Vector, VectorData, VetaError};

use crate::encoders;
use crate::footer::{MAGIC, write_footer};
use crate::meta::{
    ColumnChunk, ColumnMetaData, ColumnOrder, CompressionCodec, ConvertedType, DataPageHeader,
    DataPageHeaderV2, DictionaryPageHeader, Encoding, FieldRepetitionType, FileMetaData,
    LogicalType, PageHeader, PageType, RowGroup, SchemaElement, Statistics, TimeUnit, Type, encode,
};

/// Rows per data page at most (contract §9).
pub const PAGE_ROWS: usize = 8192;
/// Rows per row group unless `--row-group-rows` says otherwise (contract §9).
pub const DEFAULT_ROW_GROUP_ROWS: usize = 1_000_000;
/// Entries of a dictionary at most; a chunk that needs more falls back to PLAIN (contract §9).
pub const MAX_DICTIONARY_ENTRIES: usize = 65_536;

/// `--encoding` of `veta export`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueEncoding {
    Plain,
    Dict,
    Delta,
    DeltaByteArray,
}

impl ValueEncoding {
    pub const ALL: [ValueEncoding; 4] = [
        ValueEncoding::Plain,
        ValueEncoding::Dict,
        ValueEncoding::Delta,
        ValueEncoding::DeltaByteArray,
    ];

    pub fn parse(name: &str) -> Option<ValueEncoding> {
        match name {
            "plain" => Some(ValueEncoding::Plain),
            "dict" => Some(ValueEncoding::Dict),
            "delta" => Some(ValueEncoding::Delta),
            "delta-byte-array" => Some(ValueEncoding::DeltaByteArray),
            _ => None,
        }
    }
}

/// `--compression` of `veta export`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Snappy,
}

impl Compression {
    pub fn parse(name: &str) -> Option<Compression> {
        match name {
            "none" => Some(Compression::None),
            "snappy" => Some(Compression::Snappy),
            _ => None,
        }
    }

    fn codec(self) -> CompressionCodec {
        match self {
            Compression::None => CompressionCodec::Uncompressed,
            Compression::Snappy => CompressionCodec::Snappy,
        }
    }

    fn compress(self, data: Vec<u8>) -> Vec<u8> {
        match self {
            Compression::None => data,
            Compression::Snappy => snappy::compress(&data),
        }
    }
}

/// `--page-version` of `veta export`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageVersion {
    V1,
    V2,
}

impl PageVersion {
    pub fn parse(name: &str) -> Option<PageVersion> {
        match name {
            "1" => Some(PageVersion::V1),
            "2" => Some(PageVersion::V2),
            _ => None,
        }
    }

    fn file_version(self) -> i32 {
        match self {
            PageVersion::V1 => 1,
            PageVersion::V2 => 2,
        }
    }
}

/// Options of a file. The defaults are those of contract §9; it fixes none for `--encoding`,
/// so `plain` is the one that always applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriterOptions {
    pub encoding: ValueEncoding,
    pub compression: Compression,
    pub page_version: PageVersion,
    pub row_group_rows: usize,
}

impl Default for WriterOptions {
    fn default() -> Self {
        WriterOptions {
            encoding: ValueEncoding::Plain,
            compression: Compression::Snappy,
            page_version: PageVersion::V1,
            row_group_rows: DEFAULT_ROW_GROUP_ROWS,
        }
    }
}

/// The value of `created_by` (contract §9).
pub fn created_by() -> String {
    format!("veta {}", env!("CARGO_PKG_VERSION"))
}

// ---- schema ----

/// The physical type of a veta type, or the error of contract §9 for those without one.
fn physical_type(field: &Field) -> Result<Type> {
    Ok(match field.data_type {
        DataType::Boolean => Type::Boolean,
        DataType::Int16 | DataType::Int32 | DataType::Date => Type::Int32,
        DataType::Int64 | DataType::Timestamp => Type::Int64,
        DataType::Decimal { precision, .. } if precision <= MAX_DECIMAL64_PRECISION => Type::Int64,
        DataType::Float64 => Type::Double,
        DataType::Varchar => Type::ByteArray,
        DataType::Int128 | DataType::Decimal { .. } => {
            return Err(VetaError::unsupported(format!(
                "parquet export of column {} of type {}",
                field.name, field.data_type
            )));
        }
    })
}

fn schema_element(field: &Field) -> Result<SchemaElement> {
    let mut element = SchemaElement {
        type_: Some(physical_type(field)?),
        type_length: None,
        repetition_type: Some(if field.nullable {
            FieldRepetitionType::Optional
        } else {
            FieldRepetitionType::Required
        }),
        name: field.name.clone(),
        num_children: None,
        converted_type: None,
        scale: None,
        precision: None,
        field_id: None,
        logical_type: None,
    };
    match field.data_type {
        DataType::Int16 => {
            element.converted_type = Some(ConvertedType::Int16);
            element.logical_type = Some(LogicalType::Integer {
                bit_width: 16,
                is_signed: true,
            });
        }
        DataType::Decimal { precision, scale } => {
            element.converted_type = Some(ConvertedType::Decimal);
            element.scale = Some(i32::from(scale));
            element.precision = Some(i32::from(precision));
            element.logical_type = Some(LogicalType::Decimal {
                scale: i32::from(scale),
                precision: i32::from(precision),
            });
        }
        DataType::Date => {
            element.converted_type = Some(ConvertedType::Date);
            element.logical_type = Some(LogicalType::Date);
        }
        // TIMESTAMP_MILLIS means adjusted to UTC (`LogicalTypes.md`), so it has no converted type.
        DataType::Timestamp => {
            element.logical_type = Some(LogicalType::Timestamp {
                is_adjusted_to_utc: false,
                unit: TimeUnit::Millis,
            });
        }
        DataType::Varchar => {
            element.converted_type = Some(ConvertedType::Utf8);
            element.logical_type = Some(LogicalType::String);
        }
        _ => {}
    }
    Ok(element)
}

fn file_schema(schema: &Schema) -> Result<Vec<SchemaElement>> {
    let children = i32::try_from(schema.len())
        .map_err(|_| VetaError::user("too many columns for a parquet file"))?;
    let mut elements = Vec::with_capacity(schema.len() + 1);
    elements.push(SchemaElement {
        type_: None,
        type_length: None,
        repetition_type: None,
        name: "schema".to_string(),
        num_children: Some(children),
        converted_type: None,
        scale: None,
        precision: None,
        field_id: None,
        logical_type: None,
    });
    for (i, field) in schema.fields.iter().enumerate() {
        if schema.fields[..i].iter().any(|f| f.name == field.name) {
            return Err(VetaError::user(format!(
                "parquet export with two columns named {}",
                field.name
            )));
        }
        elements.push(schema_element(field)?);
    }
    Ok(elements)
}

// ---- values of a column chunk ----

/// The physical values of the non-NULL rows of a column chunk.
#[derive(Debug, PartialEq)]
enum Values<'a> {
    Bool(Vec<bool>),
    I32(Vec<i32>),
    I64(Vec<i64>),
    F64(Vec<f64>),
    Bytes(Vec<&'a [u8]>),
}

/// A dictionary and the index of each non-NULL row.
struct Dictionary<'a> {
    values: Values<'a>,
    indices: Vec<u32>,
    bit_width: u8,
}

fn mismatch(field: &Field, data: &VectorData) -> VetaError {
    VetaError::internal(format!(
        "column {} of type {} got a {} vector",
        field.name,
        field.data_type,
        data.data_type()
    ))
}

/// Maps the valid rows of every part of a fixed-width column.
fn fixed<T: Copy, U>(
    field: &Field,
    parts: &[&Vector],
    pick: impl Fn(&VectorData) -> Option<&[T]>,
    mut map: impl FnMut(T) -> Result<U>,
) -> Result<Vec<U>> {
    let rows = parts.iter().map(|part| part.len()).sum();
    let mut out = Vec::with_capacity(rows);
    for part in parts {
        let values = pick(&part.data).ok_or_else(|| mismatch(field, &part.data))?;
        match &part.validity {
            None => {
                for &value in values {
                    out.push(map(value)?);
                }
            }
            Some(validity) => {
                for (i, &value) in values.iter().enumerate() {
                    if validity.get(i) {
                        out.push(map(value)?);
                    }
                }
            }
        }
    }
    Ok(out)
}

fn timestamp_millis(seconds: i64) -> Result<i64> {
    seconds.checked_mul(1000).ok_or_else(|| {
        VetaError::user(format!(
            "timestamp of {seconds} seconds does not fit TIMESTAMP(MILLIS)"
        ))
    })
}

impl<'a> Values<'a> {
    /// The physical values of the valid rows of `parts`.
    fn gather(field: &Field, parts: &[&'a Vector]) -> Result<Values<'a>> {
        Ok(match field.data_type {
            DataType::Boolean => Values::Bool(fixed(
                field,
                parts,
                |d| match d {
                    VectorData::Boolean(v) => Some(v.as_slice()),
                    _ => None,
                },
                Ok,
            )?),
            DataType::Int16 => Values::I32(fixed(
                field,
                parts,
                |d| match d {
                    VectorData::Int16(v) => Some(v.as_slice()),
                    _ => None,
                },
                |v| Ok(i32::from(v)),
            )?),
            DataType::Int32 => Values::I32(fixed(
                field,
                parts,
                |d| match d {
                    VectorData::Int32(v) => Some(v.as_slice()),
                    _ => None,
                },
                Ok,
            )?),
            DataType::Date => Values::I32(fixed(
                field,
                parts,
                |d| match d {
                    VectorData::Date(v) => Some(v.as_slice()),
                    _ => None,
                },
                Ok,
            )?),
            DataType::Int64 => Values::I64(fixed(
                field,
                parts,
                |d| match d {
                    VectorData::Int64(v) => Some(v.as_slice()),
                    _ => None,
                },
                Ok,
            )?),
            DataType::Timestamp => Values::I64(fixed(
                field,
                parts,
                |d| match d {
                    VectorData::Timestamp(v) => Some(v.as_slice()),
                    _ => None,
                },
                timestamp_millis,
            )?),
            DataType::Decimal { scale, .. } => Values::I64(fixed(
                field,
                parts,
                |d| match d {
                    VectorData::Decimal64 {
                        values, scale: s, ..
                    } if *s == scale => Some(values.as_slice()),
                    _ => None,
                },
                Ok,
            )?),
            DataType::Float64 => Values::F64(fixed(
                field,
                parts,
                |d| match d {
                    VectorData::Float64(v) => Some(v.as_slice()),
                    _ => None,
                },
                Ok,
            )?),
            DataType::Varchar => {
                let mut out = Vec::with_capacity(parts.iter().map(|part| part.len()).sum());
                for &part in parts {
                    if !matches!(part.data, VectorData::Varchar { .. }) {
                        return Err(mismatch(field, &part.data));
                    }
                    for i in 0..part.len() {
                        if part.is_valid(i) {
                            let value = part
                                .data
                                .str_at(i)
                                .ok_or_else(|| mismatch(field, &part.data))?;
                            out.push(value);
                        }
                    }
                }
                Values::Bytes(out)
            }
            DataType::Int128 => return Err(mismatch(field, &parts[0].data)),
        })
    }

    fn len(&self) -> usize {
        match self {
            Values::Bool(v) => v.len(),
            Values::I32(v) => v.len(),
            Values::I64(v) => v.len(),
            Values::F64(v) => v.len(),
            Values::Bytes(v) => v.len(),
        }
    }

    /// PLAIN encoding of the values in `range`.
    fn plain(&self, range: Range<usize>, out: &mut Vec<u8>) {
        match self {
            Values::Bool(v) => encoders::plain_bool(&v[range], out),
            Values::I32(v) => encoders::plain_i32(&v[range], out),
            Values::I64(v) => encoders::plain_i64(&v[range], out),
            Values::F64(v) => encoders::plain_f64(&v[range], out),
            Values::Bytes(v) => encoders::plain_byte_array(&v[range], out),
        }
    }

    /// Bytes of the PLAIN encoding of the values in `range`, without encoding them.
    fn plain_len(&self, range: Range<usize>) -> u64 {
        match self {
            Values::Bool(_) => range.len().div_ceil(8) as u64,
            Values::I32(_) => range.len() as u64 * 4,
            Values::I64(_) | Values::F64(_) => range.len() as u64 * 8,
            Values::Bytes(v) => v[range].iter().map(|s| 4 + s.len() as u64).sum(),
        }
    }

    /// `(min, max)` in their PLAIN form: none for a chunk without values or with a NaN.
    fn bounds(&self) -> Option<(Vec<u8>, Vec<u8>)> {
        match self {
            Values::Bool(v) => {
                let min = v.iter().copied().min()?;
                let max = v.iter().copied().max()?;
                Some((vec![u8::from(min)], vec![u8::from(max)]))
            }
            Values::I32(v) => {
                let min = v.iter().copied().min()?;
                let max = v.iter().copied().max()?;
                Some((min.to_le_bytes().to_vec(), max.to_le_bytes().to_vec()))
            }
            Values::I64(v) => {
                let min = v.iter().copied().min()?;
                let max = v.iter().copied().max()?;
                Some((min.to_le_bytes().to_vec(), max.to_le_bytes().to_vec()))
            }
            Values::F64(v) => {
                if v.iter().any(|x| x.is_nan()) {
                    return None;
                }
                let min = v.iter().copied().min_by(f64::total_cmp)?;
                let max = v.iter().copied().max_by(f64::total_cmp)?;
                Some((min.to_le_bytes().to_vec(), max.to_le_bytes().to_vec()))
            }
            Values::Bytes(v) => {
                let min = v.iter().copied().min()?;
                let max = v.iter().copied().max()?;
                Some((min.to_vec(), max.to_vec()))
            }
        }
    }

    /// The dictionary of these values, or `None` if it needs more than
    /// [`MAX_DICTIONARY_ENTRIES`] entries. A chunk without values has none: there is nothing
    /// to put in its page.
    fn dictionary(&self) -> Option<Dictionary<'a>> {
        if self.len() == 0 {
            return None;
        }
        let (values, indices) = match self {
            Values::Bool(v) => {
                let (keys, indices) = dictionary_of(v.iter().copied())?;
                (Values::Bool(keys), indices)
            }
            Values::I32(v) => {
                let (keys, indices) = dictionary_of(v.iter().copied())?;
                (Values::I32(keys), indices)
            }
            Values::I64(v) => {
                let (keys, indices) = dictionary_of(v.iter().copied())?;
                (Values::I64(keys), indices)
            }
            // By bits: −0 and every NaN keep their exact value.
            Values::F64(v) => {
                let (keys, indices) = dictionary_of(v.iter().map(|x| x.to_bits()))?;
                (
                    Values::F64(keys.into_iter().map(f64::from_bits).collect()),
                    indices,
                )
            }
            Values::Bytes(v) => {
                let (keys, indices) = dictionary_of(v.iter().copied())?;
                (Values::Bytes(keys), indices)
            }
        };
        let bit_width = encoders::dict_bit_width(values.len());
        Some(Dictionary {
            values,
            indices,
            bit_width,
        })
    }
}

fn dictionary_of<K: Copy + Eq + Hash>(
    values: impl ExactSizeIterator<Item = K>,
) -> Option<(Vec<K>, Vec<u32>)> {
    let mut positions: HashMap<K, u32> = HashMap::new();
    let mut keys = Vec::new();
    let mut indices = Vec::with_capacity(values.len());
    for value in values {
        let index = match positions.get(&value) {
            Some(&index) => index,
            None => {
                if keys.len() == MAX_DICTIONARY_ENTRIES {
                    return None;
                }
                let index = keys.len() as u32;
                keys.push(value);
                positions.insert(value, index);
                index
            }
        };
        indices.push(index);
    }
    Some((keys, indices))
}

/// Encoding of the data pages of a chunk of `data_type` (contract §9), before the fallback
/// of a dictionary.
fn data_encoding(encoding: ValueEncoding, data_type: DataType) -> Encoding {
    let delta_strings = match encoding {
        ValueEncoding::Plain => return Encoding::Plain,
        ValueEncoding::Dict => return Encoding::RleDictionary,
        ValueEncoding::Delta => Encoding::DeltaLengthByteArray,
        ValueEncoding::DeltaByteArray => Encoding::DeltaByteArray,
    };
    match data_type {
        DataType::Int16 | DataType::Int32 | DataType::Int64 | DataType::Date => {
            Encoding::DeltaBinaryPacked
        }
        DataType::Decimal { .. } => Encoding::DeltaBinaryPacked,
        DataType::Varchar => delta_strings,
        _ => Encoding::Plain,
    }
}

// ---- file ----

fn page_size(len: usize) -> Result<i32> {
    i32::try_from(len)
        .map_err(|_| VetaError::user(format!("parquet page of {len} bytes does not fit in 2 GiB")))
}

/// Bytes a column chunk has written so far.
#[derive(Default)]
struct ChunkSizes {
    uncompressed: i64,
    compressed: i64,
}

/// Writes the rows of chunks into a file, one row group at a time.
pub struct ParquetWriter<W: Write> {
    out: W,
    position: u64,
    schema: Schema,
    elements: Vec<SchemaElement>,
    options: WriterOptions,
    pending: Vec<Chunk>,
    pending_rows: usize,
    row_groups: Vec<RowGroup>,
    num_rows: i64,
}

impl<W: Write> ParquetWriter<W> {
    /// Starts a file for `schema`. A type without a mapping (contract §9), a repeated column
    /// name or `row_group_rows` 0 are errors before anything is written.
    pub fn new(mut out: W, schema: &Schema, options: WriterOptions) -> Result<ParquetWriter<W>> {
        if options.row_group_rows == 0 {
            return Err(VetaError::user("--row-group-rows must be at least 1"));
        }
        let elements = file_schema(schema)?;
        out.write_all(MAGIC)?;
        Ok(ParquetWriter {
            out,
            position: MAGIC.len() as u64,
            schema: schema.clone(),
            elements,
            options,
            pending: Vec::new(),
            pending_rows: 0,
            row_groups: Vec::new(),
            num_rows: 0,
        })
    }

    /// Adds the rows of `chunk`; every row group it completes is written now.
    pub fn write_chunk(&mut self, chunk: Chunk) -> Result<()> {
        if chunk.num_columns() != self.schema.len() {
            return Err(VetaError::internal(format!(
                "chunk of {} columns for a parquet file of {}",
                chunk.num_columns(),
                self.schema.len()
            )));
        }
        let limit = self.options.row_group_rows;
        if chunk.len <= limit - self.pending_rows {
            if chunk.len > 0 {
                self.pending_rows += chunk.len;
                self.pending.push(chunk);
            }
        } else {
            let mut offset = 0;
            while offset < chunk.len {
                let take = (chunk.len - offset).min(limit - self.pending_rows);
                self.pending.push(chunk.slice(offset, take));
                self.pending_rows += take;
                offset += take;
                if self.pending_rows == limit {
                    self.flush_row_group()?;
                }
            }
        }
        if self.pending_rows == limit {
            self.flush_row_group()?;
        }
        Ok(())
    }

    /// Writes the last row group and the footer; returns the output, flushed.
    pub fn finish(mut self) -> Result<W> {
        self.flush_row_group()?;
        let meta = FileMetaData {
            version: self.options.page_version.file_version(),
            schema: self.elements,
            num_rows: self.num_rows,
            row_groups: self.row_groups,
            key_value_metadata: None,
            created_by: Some(created_by()),
            column_orders: Some(vec![ColumnOrder::TypeDefined; self.schema.len()]),
        };
        write_footer(&mut self.out, &meta)?;
        self.out.flush()?;
        Ok(self.out)
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<()> {
        self.out.write_all(bytes)?;
        self.position += bytes.len() as u64;
        Ok(())
    }

    fn flush_row_group(&mut self) -> Result<()> {
        if self.pending_rows == 0 {
            return Ok(());
        }
        let pending = std::mem::take(&mut self.pending);
        let rows = std::mem::take(&mut self.pending_rows);
        let start = self.position;
        let mut columns = Vec::with_capacity(self.schema.len());
        let mut total_byte_size = 0;
        let mut total_compressed_size = 0;
        for c in 0..self.schema.len() {
            let parts: Vec<&Vector> = pending.iter().map(|chunk| chunk.column(c)).collect();
            let (column, sizes) = self.write_column_chunk(c, &parts, rows)?;
            total_byte_size += sizes.uncompressed;
            total_compressed_size += sizes.compressed;
            columns.push(column);
        }
        self.row_groups.push(RowGroup {
            columns,
            total_byte_size,
            num_rows: rows as i64,
            file_offset: Some(start as i64),
            total_compressed_size: Some(total_compressed_size),
            ordinal: i16::try_from(self.row_groups.len()).ok(),
        });
        self.num_rows += rows as i64;
        Ok(())
    }

    fn write_column_chunk(
        &mut self,
        column: usize,
        parts: &[&Vector],
        rows: usize,
    ) -> Result<(ColumnChunk, ChunkSizes)> {
        let field = self.schema.field(column).clone();
        let null_count: usize = parts.iter().map(|part| part.null_count()).sum();
        if null_count > 0 && !field.nullable {
            return Err(VetaError::internal(format!(
                "NULL in the NOT NULL column {}",
                field.name
            )));
        }
        let valid: Option<Vec<bool>> = field.nullable.then(|| {
            let mut valid = Vec::with_capacity(rows);
            for part in parts {
                valid.extend((0..part.len()).map(|i| part.is_valid(i)));
            }
            valid
        });
        let values = Values::gather(&field, parts)?;
        let mut encoding = data_encoding(self.options.encoding, field.data_type);
        let dictionary = if encoding == Encoding::RleDictionary {
            let dictionary = values.dictionary();
            if dictionary.is_none() {
                encoding = Encoding::Plain;
            }
            dictionary
        } else {
            None
        };

        let mut sizes = ChunkSizes::default();
        let dictionary_page_offset = match &dictionary {
            Some(dictionary) => {
                let offset = self.position as i64;
                self.write_dictionary_page(dictionary, &mut sizes)?;
                Some(offset)
            }
            None => None,
        };
        let data_page_offset = self.position as i64;
        let mut first_value = 0;
        let mut first_row = 0;
        while first_row < rows {
            let page_rows = PAGE_ROWS.min(rows - first_row);
            let levels = valid
                .as_ref()
                .map(|valid| &valid[first_row..first_row + page_rows]);
            let present = levels.map_or(page_rows, |l| l.iter().filter(|&&v| v).count());
            let range = first_value..first_value + present;
            let mut page = Vec::new();
            encode_values(&values, dictionary.as_ref(), encoding, range, &mut page)?;
            self.write_data_page(page_rows, present, levels, encoding, page, &mut sizes)?;
            first_row += page_rows;
            first_value += present;
        }

        let mut encodings = vec![encoding];
        if dictionary.is_some() {
            encodings.push(Encoding::Plain);
        }
        if field.nullable {
            encodings.push(Encoding::Rle);
        }
        encodings.sort_by_key(|e| e.to_i32());
        let (min_value, max_value) = values.bounds().unzip();
        let exact = min_value.is_some().then_some(true);
        let meta_data = ColumnMetaData {
            type_: physical_type(&field)?,
            encodings,
            path_in_schema: vec![field.name.clone()],
            codec: self.options.compression.codec(),
            num_values: rows as i64,
            total_uncompressed_size: sizes.uncompressed,
            total_compressed_size: sizes.compressed,
            key_value_metadata: None,
            data_page_offset,
            index_page_offset: None,
            dictionary_page_offset,
            statistics: Some(Statistics {
                max: None,
                min: None,
                null_count: Some(null_count as i64),
                distinct_count: None,
                max_value,
                min_value,
                is_max_value_exact: exact,
                is_min_value_exact: exact,
            }),
        };
        let chunk = ColumnChunk {
            file_path: None,
            file_offset: 0,
            meta_data: Some(meta_data),
        };
        Ok((chunk, sizes))
    }

    fn write_page(
        &mut self,
        header: &PageHeader,
        body: &[&[u8]],
        uncompressed: usize,
        sizes: &mut ChunkSizes,
    ) -> Result<()> {
        let header_bytes = encode(header);
        self.write_bytes(&header_bytes)?;
        let mut stored = 0;
        for part in body {
            self.write_bytes(part)?;
            stored += part.len();
        }
        sizes.uncompressed += (header_bytes.len() + uncompressed) as i64;
        sizes.compressed += (header_bytes.len() + stored) as i64;
        Ok(())
    }

    fn write_dictionary_page(
        &mut self,
        dictionary: &Dictionary<'_>,
        sizes: &mut ChunkSizes,
    ) -> Result<()> {
        let entries = dictionary.values.len();
        page_size_of(dictionary.values.plain_len(0..entries))?;
        let mut plain = Vec::new();
        dictionary.values.plain(0..entries, &mut plain);
        let uncompressed = plain.len();
        let stored = self.options.compression.compress(plain);
        let header = PageHeader {
            type_: PageType::DictionaryPage,
            uncompressed_page_size: page_size(uncompressed)?,
            compressed_page_size: page_size(stored.len())?,
            crc: None,
            data_page_header: None,
            dictionary_page_header: Some(DictionaryPageHeader {
                num_values: entries as i32,
                encoding: Encoding::Plain,
                is_sorted: None,
            }),
            data_page_header_v2: None,
        };
        self.write_page(&header, &[&stored], uncompressed, sizes)
    }

    fn write_data_page(
        &mut self,
        rows: usize,
        present: usize,
        levels: Option<&[bool]>,
        encoding: Encoding,
        values: Vec<u8>,
        sizes: &mut ChunkSizes,
    ) -> Result<()> {
        let mut level_bytes = Vec::new();
        if let Some(levels) = levels {
            encoders::encode_def_levels(levels, &mut level_bytes);
        }
        let uncompressed = level_bytes.len() + 4 + values.len();
        page_size(uncompressed)?;
        match self.options.page_version {
            PageVersion::V1 => {
                let mut body = Vec::with_capacity(uncompressed);
                if levels.is_some() {
                    body.extend_from_slice(&(level_bytes.len() as u32).to_le_bytes());
                    body.extend_from_slice(&level_bytes);
                }
                body.extend_from_slice(&values);
                let uncompressed = body.len();
                let stored = self.options.compression.compress(body);
                let header = PageHeader {
                    type_: PageType::DataPage,
                    uncompressed_page_size: page_size(uncompressed)?,
                    compressed_page_size: page_size(stored.len())?,
                    crc: None,
                    data_page_header: Some(DataPageHeader {
                        num_values: rows as i32,
                        encoding,
                        definition_level_encoding: Encoding::Rle,
                        repetition_level_encoding: Encoding::Rle,
                        statistics: None,
                    }),
                    dictionary_page_header: None,
                    data_page_header_v2: None,
                };
                self.write_page(&header, &[&stored], uncompressed, sizes)
            }
            PageVersion::V2 => {
                let uncompressed = level_bytes.len() + values.len();
                // A page of only NULLs has no values to compress.
                let is_compressed =
                    self.options.compression != Compression::None && !values.is_empty();
                let stored = if is_compressed {
                    self.options.compression.compress(values)
                } else {
                    values
                };
                let header = PageHeader {
                    type_: PageType::DataPageV2,
                    uncompressed_page_size: page_size(uncompressed)?,
                    compressed_page_size: page_size(level_bytes.len() + stored.len())?,
                    crc: None,
                    data_page_header: None,
                    dictionary_page_header: None,
                    data_page_header_v2: Some(DataPageHeaderV2 {
                        num_values: rows as i32,
                        num_nulls: (rows - present) as i32,
                        num_rows: rows as i32,
                        encoding,
                        definition_levels_byte_length: page_size(level_bytes.len())?,
                        repetition_levels_byte_length: 0,
                        is_compressed: Some(is_compressed),
                        statistics: None,
                    }),
                };
                self.write_page(&header, &[&level_bytes, &stored], uncompressed, sizes)
            }
        }
    }
}

fn page_size_of(len: u64) -> Result<()> {
    usize::try_from(len)
        .map_err(|_| VetaError::user("parquet page does not fit in memory"))
        .and_then(page_size)
        .map(|_| ())
}

/// Encodes the non-NULL values of one page; a page too large for its header is an error
/// before it is built.
fn encode_values(
    values: &Values<'_>,
    dictionary: Option<&Dictionary<'_>>,
    encoding: Encoding,
    range: Range<usize>,
    out: &mut Vec<u8>,
) -> Result<()> {
    if encoding != Encoding::RleDictionary {
        page_size_of(values.plain_len(range.clone()))?;
    }
    match (encoding, values, dictionary) {
        (Encoding::Plain, _, _) => values.plain(range, out),
        (Encoding::RleDictionary, _, Some(dictionary)) => {
            encoders::encode_dict_indices(&dictionary.indices[range], dictionary.bit_width, out)
        }
        (Encoding::DeltaBinaryPacked, Values::I32(v), _) => {
            encoders::encode_delta_i32(&v[range], out)
        }
        (Encoding::DeltaBinaryPacked, Values::I64(v), _) => {
            encoders::encode_delta_i64(&v[range], out)
        }
        (Encoding::DeltaLengthByteArray, Values::Bytes(v), _) => {
            encoders::encode_delta_length_byte_array(&v[range], out)
        }
        (Encoding::DeltaByteArray, Values::Bytes(v), _) => {
            encoders::encode_delta_byte_array(&v[range], out)
        }
        _ => {
            return Err(VetaError::internal(format!(
                "encoding {} over other values",
                encoding.name()
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_parse_their_names() {
        for (name, encoding) in [
            ("plain", ValueEncoding::Plain),
            ("dict", ValueEncoding::Dict),
            ("delta", ValueEncoding::Delta),
            ("delta-byte-array", ValueEncoding::DeltaByteArray),
        ] {
            assert_eq!(ValueEncoding::parse(name), Some(encoding));
        }
        assert_eq!(ValueEncoding::parse("rle"), None);
        assert_eq!(Compression::parse("none"), Some(Compression::None));
        assert_eq!(Compression::parse("snappy"), Some(Compression::Snappy));
        assert_eq!(Compression::parse("zstd"), None);
        assert_eq!(PageVersion::parse("1"), Some(PageVersion::V1));
        assert_eq!(PageVersion::parse("2"), Some(PageVersion::V2));
        assert_eq!(PageVersion::parse("3"), None);
        let options = WriterOptions::default();
        assert_eq!(options.compression, Compression::Snappy);
        assert_eq!(options.page_version, PageVersion::V1);
        assert_eq!(options.row_group_rows, 1_000_000);
    }

    #[test]
    fn delta_applies_as_the_contract_says() {
        let delta = |t| data_encoding(ValueEncoding::Delta, t);
        let dba = |t| data_encoding(ValueEncoding::DeltaByteArray, t);
        for t in [
            DataType::Int16,
            DataType::Int32,
            DataType::Int64,
            DataType::Date,
            DataType::Decimal {
                precision: 10,
                scale: 2,
            },
        ] {
            assert_eq!(delta(t), Encoding::DeltaBinaryPacked);
            assert_eq!(dba(t), Encoding::DeltaBinaryPacked);
        }
        for t in [DataType::Boolean, DataType::Float64, DataType::Timestamp] {
            assert_eq!(delta(t), Encoding::Plain);
            assert_eq!(dba(t), Encoding::Plain);
        }
        assert_eq!(delta(DataType::Varchar), Encoding::DeltaLengthByteArray);
        assert_eq!(dba(DataType::Varchar), Encoding::DeltaByteArray);
    }

    #[test]
    fn dictionary_stops_past_its_limit() {
        let fits: Vec<i64> = (0..MAX_DICTIONARY_ENTRIES as i64).collect();
        let dictionary = Values::I64(fits).dictionary().unwrap();
        assert_eq!(dictionary.values.len(), MAX_DICTIONARY_ENTRIES);
        assert_eq!(dictionary.bit_width, 16);
        let over: Vec<i64> = (0..=MAX_DICTIONARY_ENTRIES as i64).collect();
        assert!(Values::I64(over).dictionary().is_none());
        assert!(Values::I64(Vec::new()).dictionary().is_none());
    }

    #[test]
    fn dictionary_of_doubles_keeps_the_bits() {
        let dictionary = Values::F64(vec![0.0, -0.0, 0.0, f64::NAN, 1.5])
            .dictionary()
            .unwrap();
        let Values::F64(keys) = &dictionary.values else {
            panic!("not doubles");
        };
        assert_eq!(keys.len(), 4);
        assert_eq!(keys[1].to_bits(), (-0.0f64).to_bits());
        assert_eq!(dictionary.indices, [0, 1, 0, 2, 3]);
    }

    #[test]
    fn bounds_follow_the_physical_order() {
        let bytes = |v: f64| v.to_le_bytes().to_vec();
        assert_eq!(Values::I32(Vec::new()).bounds(), None);
        assert_eq!(
            Values::Bool(vec![true, true]).bounds(),
            Some((vec![1], vec![1]))
        );
        assert_eq!(
            Values::I32(vec![3, -7, 5]).bounds(),
            Some(((-7i32).to_le_bytes().to_vec(), 5i32.to_le_bytes().to_vec()))
        );
        let strings: Vec<&[u8]> = vec![b"b", b"\xff", b"", b"a"];
        assert_eq!(
            Values::Bytes(strings).bounds(),
            Some((Vec::new(), vec![0xff]))
        );
        assert_eq!(
            Values::F64(vec![1.0, -0.0, 0.0]).bounds(),
            Some((bytes(-0.0), bytes(1.0)))
        );
        assert_eq!(
            Values::F64(vec![-1.0, 0.0]).bounds(),
            Some((bytes(-1.0), bytes(0.0)))
        );
        assert_eq!(Values::F64(vec![1.0, f64::NAN]).bounds(), None);
    }
}
