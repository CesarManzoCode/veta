//! Parquet metadata structs of `parquet.thrift` (parquet-format 2.14.0, contract §9), read
//! and written with the compact protocol of [`crate::thrift`].
//!
//! Field ids and enum values follow that `parquet.thrift`. Only the fields the project
//! uses are kept; any other field (known to the spec or not) is skipped when reading.
//! Enums keep values they do not know as `Unknown(i32)`, so a footer from a newer writer
//! still decodes and only the code that needs the value decides whether it is an error.
//! Missing required fields and fields of the wrong type are errors.

use crate::invalid;
use crate::thrift::{CompactReader, CompactWriter, ttype};
use veta_core::Result;

/// A struct encoded with the compact protocol.
pub trait ThriftStruct: Sized {
    fn read(r: &mut CompactReader<'_>) -> Result<Self>;
    fn write(&self, w: &mut CompactWriter);
}

/// Decodes a `T` at the start of `buf`; returns it and the bytes it took.
pub fn decode<T: ThriftStruct>(buf: &[u8]) -> Result<(T, usize)> {
    let mut r = CompactReader::new(buf);
    let v = T::read(&mut r)?;
    Ok((v, r.position()))
}

pub fn encode<T: ThriftStruct>(v: &T) -> Vec<u8> {
    let mut w = CompactWriter::new();
    v.write(&mut w);
    w.into_bytes()
}

macro_rules! thrift_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident = $val:literal => $label:literal),* $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name {
            $($variant,)*
            /// A value this version of `parquet.thrift` does not define.
            Unknown(i32),
        }

        impl $name {
            pub fn from_i32(v: i32) -> Self {
                match v {
                    $($val => $name::$variant,)*
                    other => $name::Unknown(other),
                }
            }

            pub fn to_i32(self) -> i32 {
                match self {
                    $($name::$variant => $val,)*
                    $name::Unknown(v) => v,
                }
            }

            /// Name as spelled in `parquet.thrift`.
            pub fn name(self) -> String {
                match self {
                    $($name::$variant => $label.to_string(),)*
                    $name::Unknown(v) => format!("UNKNOWN({v})"),
                }
            }
        }
    };
}

thrift_enum! {
    /// Physical type.
    Type {
        Boolean = 0 => "BOOLEAN",
        Int32 = 1 => "INT32",
        Int64 = 2 => "INT64",
        Int96 = 3 => "INT96",
        Float = 4 => "FLOAT",
        Double = 5 => "DOUBLE",
        ByteArray = 6 => "BYTE_ARRAY",
        FixedLenByteArray = 7 => "FIXED_LEN_BYTE_ARRAY",
    }
}

thrift_enum! {
    ConvertedType {
        Utf8 = 0 => "UTF8",
        Map = 1 => "MAP",
        MapKeyValue = 2 => "MAP_KEY_VALUE",
        List = 3 => "LIST",
        Enum = 4 => "ENUM",
        Decimal = 5 => "DECIMAL",
        Date = 6 => "DATE",
        TimeMillis = 7 => "TIME_MILLIS",
        TimeMicros = 8 => "TIME_MICROS",
        TimestampMillis = 9 => "TIMESTAMP_MILLIS",
        TimestampMicros = 10 => "TIMESTAMP_MICROS",
        Uint8 = 11 => "UINT_8",
        Uint16 = 12 => "UINT_16",
        Uint32 = 13 => "UINT_32",
        Uint64 = 14 => "UINT_64",
        Int8 = 15 => "INT_8",
        Int16 = 16 => "INT_16",
        Int32 = 17 => "INT_32",
        Int64 = 18 => "INT_64",
        Json = 19 => "JSON",
        Bson = 20 => "BSON",
        Interval = 21 => "INTERVAL",
    }
}

thrift_enum! {
    FieldRepetitionType {
        Required = 0 => "REQUIRED",
        Optional = 1 => "OPTIONAL",
        Repeated = 2 => "REPEATED",
    }
}

thrift_enum! {
    Encoding {
        Plain = 0 => "PLAIN",
        PlainDictionary = 2 => "PLAIN_DICTIONARY",
        Rle = 3 => "RLE",
        BitPacked = 4 => "BIT_PACKED",
        DeltaBinaryPacked = 5 => "DELTA_BINARY_PACKED",
        DeltaLengthByteArray = 6 => "DELTA_LENGTH_BYTE_ARRAY",
        DeltaByteArray = 7 => "DELTA_BYTE_ARRAY",
        RleDictionary = 8 => "RLE_DICTIONARY",
        ByteStreamSplit = 9 => "BYTE_STREAM_SPLIT",
        Alp = 10 => "ALP",
    }
}

thrift_enum! {
    CompressionCodec {
        Uncompressed = 0 => "UNCOMPRESSED",
        Snappy = 1 => "SNAPPY",
        Gzip = 2 => "GZIP",
        Lzo = 3 => "LZO",
        Brotli = 4 => "BROTLI",
        Lz4 = 5 => "LZ4",
        Zstd = 6 => "ZSTD",
        Lz4Raw = 7 => "LZ4_RAW",
    }
}

thrift_enum! {
    PageType {
        DataPage = 0 => "DATA_PAGE",
        IndexPage = 1 => "INDEX_PAGE",
        DictionaryPage = 2 => "DICTIONARY_PAGE",
        DataPageV2 = 3 => "DATA_PAGE_V2",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeUnit {
    Millis,
    Micros,
    Nanos,
}

/// The `LogicalType` union. Variants the project does not use (MAP, LIST, TIME, JSON, …)
/// read as `Unknown(field id)`; writing `Unknown(id)` emits an empty struct for that id,
/// which is valid only for the variants whose struct is empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalType {
    String,
    Enum,
    Decimal {
        scale: i32,
        precision: i32,
    },
    Date,
    Timestamp {
        is_adjusted_to_utc: bool,
        unit: TimeUnit,
    },
    Integer {
        bit_width: i8,
        is_signed: bool,
    },
    Unknown(i16),
}

/// The `ColumnOrder` union, all of whose variants are empty structs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnOrder {
    TypeDefined,
    Other(i16),
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Statistics {
    pub max: Option<Vec<u8>>,
    pub min: Option<Vec<u8>>,
    pub null_count: Option<i64>,
    pub distinct_count: Option<i64>,
    pub max_value: Option<Vec<u8>>,
    pub min_value: Option<Vec<u8>>,
    pub is_max_value_exact: Option<bool>,
    pub is_min_value_exact: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaElement {
    pub type_: Option<Type>,
    pub type_length: Option<i32>,
    pub repetition_type: Option<FieldRepetitionType>,
    pub name: String,
    pub num_children: Option<i32>,
    pub converted_type: Option<ConvertedType>,
    pub scale: Option<i32>,
    pub precision: Option<i32>,
    pub field_id: Option<i32>,
    pub logical_type: Option<LogicalType>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyValue {
    pub key: String,
    pub value: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMetaData {
    pub type_: Type,
    pub encodings: Vec<Encoding>,
    pub path_in_schema: Vec<String>,
    pub codec: CompressionCodec,
    pub num_values: i64,
    pub total_uncompressed_size: i64,
    pub total_compressed_size: i64,
    pub key_value_metadata: Option<Vec<KeyValue>>,
    pub data_page_offset: i64,
    pub index_page_offset: Option<i64>,
    pub dictionary_page_offset: Option<i64>,
    pub statistics: Option<Statistics>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnChunk {
    pub file_path: Option<String>,
    /// Required with default 0 in `parquet.thrift`; a missing value reads as 0.
    pub file_offset: i64,
    pub meta_data: Option<ColumnMetaData>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RowGroup {
    pub columns: Vec<ColumnChunk>,
    pub total_byte_size: i64,
    pub num_rows: i64,
    pub file_offset: Option<i64>,
    pub total_compressed_size: Option<i64>,
    pub ordinal: Option<i16>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMetaData {
    pub version: i32,
    pub schema: Vec<SchemaElement>,
    pub num_rows: i64,
    pub row_groups: Vec<RowGroup>,
    pub key_value_metadata: Option<Vec<KeyValue>>,
    pub created_by: Option<String>,
    pub column_orders: Option<Vec<ColumnOrder>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataPageHeader {
    pub num_values: i32,
    pub encoding: Encoding,
    pub definition_level_encoding: Encoding,
    pub repetition_level_encoding: Encoding,
    pub statistics: Option<Statistics>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DictionaryPageHeader {
    pub num_values: i32,
    pub encoding: Encoding,
    pub is_sorted: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataPageHeaderV2 {
    pub num_values: i32,
    pub num_nulls: i32,
    pub num_rows: i32,
    pub encoding: Encoding,
    pub definition_levels_byte_length: i32,
    pub repetition_levels_byte_length: i32,
    /// Absent means compressed (default `true` in `parquet.thrift`).
    pub is_compressed: Option<bool>,
    pub statistics: Option<Statistics>,
}

impl DataPageHeaderV2 {
    pub fn is_compressed(&self) -> bool {
        self.is_compressed.unwrap_or(true)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageHeader {
    pub type_: PageType,
    pub uncompressed_page_size: i32,
    pub compressed_page_size: i32,
    pub crc: Option<i32>,
    pub data_page_header: Option<DataPageHeader>,
    pub dictionary_page_header: Option<DictionaryPageHeader>,
    pub data_page_header_v2: Option<DataPageHeaderV2>,
}

// ---- reading helpers ----

fn want(actual: u8, expected: u8, what: &str) -> Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(invalid(format!(
            "{what} has thrift type {actual}, expected {expected}"
        )))
    }
}

fn required<T>(v: Option<T>, what: &str) -> Result<T> {
    v.ok_or_else(|| invalid(format!("missing required field {what}")))
}

fn read_list<'a, T>(
    r: &mut CompactReader<'a>,
    t: u8,
    elem: u8,
    what: &str,
    mut f: impl FnMut(&mut CompactReader<'a>) -> Result<T>,
) -> Result<Vec<T>> {
    want(t, ttype::LIST, what)?;
    let (et, n) = r.list_header()?;
    if n > 0 {
        want(et, elem, what)?;
    }
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(f(r)?);
    }
    Ok(out)
}

fn read_i32_field(r: &mut CompactReader<'_>, t: u8, what: &str) -> Result<i32> {
    want(t, ttype::I32, what)?;
    r.read_i32()
}

fn read_i64_field(r: &mut CompactReader<'_>, t: u8, what: &str) -> Result<i64> {
    want(t, ttype::I64, what)?;
    r.read_i64()
}

fn read_bool_field(r: &mut CompactReader<'_>, t: u8, what: &str) -> Result<bool> {
    want(t, ttype::BOOL, what)?;
    r.read_bool()
}

fn read_binary_field(r: &mut CompactReader<'_>, t: u8, what: &str) -> Result<Vec<u8>> {
    want(t, ttype::BINARY, what)?;
    Ok(r.read_binary()?.to_vec())
}

fn read_string_field(r: &mut CompactReader<'_>, t: u8, what: &str) -> Result<String> {
    want(t, ttype::BINARY, what)?;
    r.read_string()
}

fn read_struct_field<T: ThriftStruct>(r: &mut CompactReader<'_>, t: u8, what: &str) -> Result<T> {
    want(t, ttype::STRUCT, what)?;
    T::read(r)
}

/// Skips an empty-struct value (union variants such as `StringType {}`).
fn read_empty_struct(r: &mut CompactReader<'_>, t: u8, what: &str) -> Result<()> {
    want(t, ttype::STRUCT, what)?;
    r.skip(t)
}

fn write_empty_struct(w: &mut CompactWriter, id: i16) {
    w.field_struct(id);
    w.struct_begin();
    w.struct_end();
}

fn write_struct_list<T: ThriftStruct>(w: &mut CompactWriter, id: i16, items: &[T]) {
    w.field_list(id, ttype::STRUCT, items.len());
    for item in items {
        item.write(w);
    }
}

fn write_string_list(w: &mut CompactWriter, id: i16, items: &[String]) {
    w.field_list(id, ttype::BINARY, items.len());
    for item in items {
        w.write_binary(item.as_bytes());
    }
}

// ---- unions ----

impl TimeUnit {
    fn read(r: &mut CompactReader<'_>) -> Result<Self> {
        let mut out = None;
        r.struct_begin()?;
        while let Some((id, t)) = r.field_header()? {
            let unit = match id {
                1 => TimeUnit::Millis,
                2 => TimeUnit::Micros,
                3 => TimeUnit::Nanos,
                _ => return Err(invalid(format!("unknown TimeUnit variant {id}"))),
            };
            read_empty_struct(r, t, "TimeUnit")?;
            if out.replace(unit).is_some() {
                return Err(invalid("TimeUnit union with more than one field"));
            }
        }
        r.struct_end();
        required(out, "TimeUnit")
    }

    fn write(self, w: &mut CompactWriter) {
        let id = match self {
            TimeUnit::Millis => 1,
            TimeUnit::Micros => 2,
            TimeUnit::Nanos => 3,
        };
        w.struct_begin();
        write_empty_struct(w, id);
        w.struct_end();
    }
}

impl ThriftStruct for LogicalType {
    fn read(r: &mut CompactReader<'_>) -> Result<Self> {
        let mut out = None;
        r.struct_begin()?;
        while let Some((id, t)) = r.field_header()? {
            let v = match id {
                1 => {
                    read_empty_struct(r, t, "LogicalType.STRING")?;
                    LogicalType::String
                }
                4 => {
                    read_empty_struct(r, t, "LogicalType.ENUM")?;
                    LogicalType::Enum
                }
                5 => {
                    want(t, ttype::STRUCT, "LogicalType.DECIMAL")?;
                    read_decimal_type(r)?
                }
                6 => {
                    read_empty_struct(r, t, "LogicalType.DATE")?;
                    LogicalType::Date
                }
                8 => {
                    want(t, ttype::STRUCT, "LogicalType.TIMESTAMP")?;
                    read_timestamp_type(r)?
                }
                10 => {
                    want(t, ttype::STRUCT, "LogicalType.INTEGER")?;
                    read_int_type(r)?
                }
                _ => {
                    r.skip(t)?;
                    LogicalType::Unknown(id)
                }
            };
            if out.replace(v).is_some() {
                return Err(invalid("LogicalType union with more than one field"));
            }
        }
        r.struct_end();
        required(out, "LogicalType")
    }

    fn write(&self, w: &mut CompactWriter) {
        w.struct_begin();
        match *self {
            LogicalType::String => write_empty_struct(w, 1),
            LogicalType::Enum => write_empty_struct(w, 4),
            LogicalType::Decimal { scale, precision } => {
                w.field_struct(5);
                w.struct_begin();
                w.field_i32(1, scale);
                w.field_i32(2, precision);
                w.struct_end();
            }
            LogicalType::Date => write_empty_struct(w, 6),
            LogicalType::Timestamp {
                is_adjusted_to_utc,
                unit,
            } => {
                w.field_struct(8);
                w.struct_begin();
                w.field_bool(1, is_adjusted_to_utc);
                w.field_struct(2);
                unit.write(w);
                w.struct_end();
            }
            LogicalType::Integer {
                bit_width,
                is_signed,
            } => {
                w.field_struct(10);
                w.struct_begin();
                w.field_i8(1, bit_width);
                w.field_bool(2, is_signed);
                w.struct_end();
            }
            LogicalType::Unknown(id) => write_empty_struct(w, id),
        }
        w.struct_end();
    }
}

fn read_decimal_type(r: &mut CompactReader<'_>) -> Result<LogicalType> {
    let (mut scale, mut precision) = (None, None);
    r.struct_begin()?;
    while let Some((id, t)) = r.field_header()? {
        match id {
            1 => scale = Some(read_i32_field(r, t, "DecimalType.scale")?),
            2 => precision = Some(read_i32_field(r, t, "DecimalType.precision")?),
            _ => r.skip(t)?,
        }
    }
    r.struct_end();
    Ok(LogicalType::Decimal {
        scale: required(scale, "DecimalType.scale")?,
        precision: required(precision, "DecimalType.precision")?,
    })
}

fn read_timestamp_type(r: &mut CompactReader<'_>) -> Result<LogicalType> {
    let (mut utc, mut unit) = (None, None);
    r.struct_begin()?;
    while let Some((id, t)) = r.field_header()? {
        match id {
            1 => utc = Some(read_bool_field(r, t, "TimestampType.isAdjustedToUTC")?),
            2 => {
                want(t, ttype::STRUCT, "TimestampType.unit")?;
                unit = Some(TimeUnit::read(r)?);
            }
            _ => r.skip(t)?,
        }
    }
    r.struct_end();
    Ok(LogicalType::Timestamp {
        is_adjusted_to_utc: required(utc, "TimestampType.isAdjustedToUTC")?,
        unit: required(unit, "TimestampType.unit")?,
    })
}

fn read_int_type(r: &mut CompactReader<'_>) -> Result<LogicalType> {
    let (mut bits, mut signed) = (None, None);
    r.struct_begin()?;
    while let Some((id, t)) = r.field_header()? {
        match id {
            1 => {
                want(t, ttype::I8, "IntType.bitWidth")?;
                bits = Some(r.read_i8()?);
            }
            2 => signed = Some(read_bool_field(r, t, "IntType.isSigned")?),
            _ => r.skip(t)?,
        }
    }
    r.struct_end();
    Ok(LogicalType::Integer {
        bit_width: required(bits, "IntType.bitWidth")?,
        is_signed: required(signed, "IntType.isSigned")?,
    })
}

impl ThriftStruct for ColumnOrder {
    fn read(r: &mut CompactReader<'_>) -> Result<Self> {
        let mut out = None;
        r.struct_begin()?;
        while let Some((id, t)) = r.field_header()? {
            r.skip(t)?;
            let v = if id == 1 {
                ColumnOrder::TypeDefined
            } else {
                ColumnOrder::Other(id)
            };
            if out.replace(v).is_some() {
                return Err(invalid("ColumnOrder union with more than one field"));
            }
        }
        r.struct_end();
        required(out, "ColumnOrder")
    }

    fn write(&self, w: &mut CompactWriter) {
        let id = match *self {
            ColumnOrder::TypeDefined => 1,
            ColumnOrder::Other(id) => id,
        };
        w.struct_begin();
        write_empty_struct(w, id);
        w.struct_end();
    }
}

// ---- structs ----

impl ThriftStruct for Statistics {
    fn read(r: &mut CompactReader<'_>) -> Result<Self> {
        let mut s = Statistics::default();
        r.struct_begin()?;
        while let Some((id, t)) = r.field_header()? {
            match id {
                1 => s.max = Some(read_binary_field(r, t, "Statistics.max")?),
                2 => s.min = Some(read_binary_field(r, t, "Statistics.min")?),
                3 => s.null_count = Some(read_i64_field(r, t, "Statistics.null_count")?),
                4 => s.distinct_count = Some(read_i64_field(r, t, "Statistics.distinct_count")?),
                5 => s.max_value = Some(read_binary_field(r, t, "Statistics.max_value")?),
                6 => s.min_value = Some(read_binary_field(r, t, "Statistics.min_value")?),
                7 => {
                    s.is_max_value_exact =
                        Some(read_bool_field(r, t, "Statistics.is_max_value_exact")?)
                }
                8 => {
                    s.is_min_value_exact =
                        Some(read_bool_field(r, t, "Statistics.is_min_value_exact")?)
                }
                _ => r.skip(t)?,
            }
        }
        r.struct_end();
        Ok(s)
    }

    fn write(&self, w: &mut CompactWriter) {
        w.struct_begin();
        if let Some(v) = &self.max {
            w.field_binary(1, v);
        }
        if let Some(v) = &self.min {
            w.field_binary(2, v);
        }
        if let Some(v) = self.null_count {
            w.field_i64(3, v);
        }
        if let Some(v) = self.distinct_count {
            w.field_i64(4, v);
        }
        if let Some(v) = &self.max_value {
            w.field_binary(5, v);
        }
        if let Some(v) = &self.min_value {
            w.field_binary(6, v);
        }
        if let Some(v) = self.is_max_value_exact {
            w.field_bool(7, v);
        }
        if let Some(v) = self.is_min_value_exact {
            w.field_bool(8, v);
        }
        w.struct_end();
    }
}

impl ThriftStruct for SchemaElement {
    fn read(r: &mut CompactReader<'_>) -> Result<Self> {
        let mut type_ = None;
        let mut type_length = None;
        let mut repetition_type = None;
        let mut name = None;
        let mut num_children = None;
        let mut converted_type = None;
        let mut scale = None;
        let mut precision = None;
        let mut field_id = None;
        let mut logical_type = None;
        r.struct_begin()?;
        while let Some((id, t)) = r.field_header()? {
            match id {
                1 => type_ = Some(Type::from_i32(read_i32_field(r, t, "SchemaElement.type")?)),
                2 => type_length = Some(read_i32_field(r, t, "SchemaElement.type_length")?),
                3 => {
                    repetition_type = Some(FieldRepetitionType::from_i32(read_i32_field(
                        r,
                        t,
                        "SchemaElement.repetition_type",
                    )?))
                }
                4 => name = Some(read_string_field(r, t, "SchemaElement.name")?),
                5 => num_children = Some(read_i32_field(r, t, "SchemaElement.num_children")?),
                6 => {
                    converted_type = Some(ConvertedType::from_i32(read_i32_field(
                        r,
                        t,
                        "SchemaElement.converted_type",
                    )?))
                }
                7 => scale = Some(read_i32_field(r, t, "SchemaElement.scale")?),
                8 => precision = Some(read_i32_field(r, t, "SchemaElement.precision")?),
                9 => field_id = Some(read_i32_field(r, t, "SchemaElement.field_id")?),
                10 => logical_type = Some(read_struct_field(r, t, "SchemaElement.logicalType")?),
                _ => r.skip(t)?,
            }
        }
        r.struct_end();
        Ok(SchemaElement {
            type_,
            type_length,
            repetition_type,
            name: required(name, "SchemaElement.name")?,
            num_children,
            converted_type,
            scale,
            precision,
            field_id,
            logical_type,
        })
    }

    fn write(&self, w: &mut CompactWriter) {
        w.struct_begin();
        if let Some(v) = self.type_ {
            w.field_i32(1, v.to_i32());
        }
        if let Some(v) = self.type_length {
            w.field_i32(2, v);
        }
        if let Some(v) = self.repetition_type {
            w.field_i32(3, v.to_i32());
        }
        w.field_binary(4, self.name.as_bytes());
        if let Some(v) = self.num_children {
            w.field_i32(5, v);
        }
        if let Some(v) = self.converted_type {
            w.field_i32(6, v.to_i32());
        }
        if let Some(v) = self.scale {
            w.field_i32(7, v);
        }
        if let Some(v) = self.precision {
            w.field_i32(8, v);
        }
        if let Some(v) = self.field_id {
            w.field_i32(9, v);
        }
        if let Some(v) = &self.logical_type {
            w.field_struct(10);
            v.write(w);
        }
        w.struct_end();
    }
}

impl ThriftStruct for KeyValue {
    fn read(r: &mut CompactReader<'_>) -> Result<Self> {
        let (mut key, mut value) = (None, None);
        r.struct_begin()?;
        while let Some((id, t)) = r.field_header()? {
            match id {
                1 => key = Some(read_string_field(r, t, "KeyValue.key")?),
                2 => value = Some(read_string_field(r, t, "KeyValue.value")?),
                _ => r.skip(t)?,
            }
        }
        r.struct_end();
        Ok(KeyValue {
            key: required(key, "KeyValue.key")?,
            value,
        })
    }

    fn write(&self, w: &mut CompactWriter) {
        w.struct_begin();
        w.field_binary(1, self.key.as_bytes());
        if let Some(v) = &self.value {
            w.field_binary(2, v.as_bytes());
        }
        w.struct_end();
    }
}

impl ThriftStruct for ColumnMetaData {
    fn read(r: &mut CompactReader<'_>) -> Result<Self> {
        let mut type_ = None;
        let mut encodings = None;
        let mut path_in_schema = None;
        let mut codec = None;
        let mut num_values = None;
        let mut total_uncompressed_size = None;
        let mut total_compressed_size = None;
        let mut key_value_metadata = None;
        let mut data_page_offset = None;
        let mut index_page_offset = None;
        let mut dictionary_page_offset = None;
        let mut statistics = None;
        r.struct_begin()?;
        while let Some((id, t)) = r.field_header()? {
            match id {
                1 => type_ = Some(Type::from_i32(read_i32_field(r, t, "ColumnMetaData.type")?)),
                2 => {
                    encodings = Some(read_list(
                        r,
                        t,
                        ttype::I32,
                        "ColumnMetaData.encodings",
                        |r| Ok(Encoding::from_i32(r.read_i32()?)),
                    )?)
                }
                3 => {
                    path_in_schema = Some(read_list(
                        r,
                        t,
                        ttype::BINARY,
                        "ColumnMetaData.path_in_schema",
                        |r| r.read_string(),
                    )?)
                }
                4 => {
                    codec = Some(CompressionCodec::from_i32(read_i32_field(
                        r,
                        t,
                        "ColumnMetaData.codec",
                    )?))
                }
                5 => num_values = Some(read_i64_field(r, t, "ColumnMetaData.num_values")?),
                6 => {
                    total_uncompressed_size = Some(read_i64_field(
                        r,
                        t,
                        "ColumnMetaData.total_uncompressed_size",
                    )?)
                }
                7 => {
                    total_compressed_size = Some(read_i64_field(
                        r,
                        t,
                        "ColumnMetaData.total_compressed_size",
                    )?)
                }
                8 => {
                    key_value_metadata = Some(read_list(
                        r,
                        t,
                        ttype::STRUCT,
                        "ColumnMetaData.key_value_metadata",
                        KeyValue::read,
                    )?)
                }
                9 => {
                    data_page_offset =
                        Some(read_i64_field(r, t, "ColumnMetaData.data_page_offset")?)
                }
                10 => {
                    index_page_offset =
                        Some(read_i64_field(r, t, "ColumnMetaData.index_page_offset")?)
                }
                11 => {
                    dictionary_page_offset = Some(read_i64_field(
                        r,
                        t,
                        "ColumnMetaData.dictionary_page_offset",
                    )?)
                }
                12 => statistics = Some(read_struct_field(r, t, "ColumnMetaData.statistics")?),
                _ => r.skip(t)?,
            }
        }
        r.struct_end();
        Ok(ColumnMetaData {
            type_: required(type_, "ColumnMetaData.type")?,
            encodings: required(encodings, "ColumnMetaData.encodings")?,
            path_in_schema: required(path_in_schema, "ColumnMetaData.path_in_schema")?,
            codec: required(codec, "ColumnMetaData.codec")?,
            num_values: required(num_values, "ColumnMetaData.num_values")?,
            total_uncompressed_size: required(
                total_uncompressed_size,
                "ColumnMetaData.total_uncompressed_size",
            )?,
            total_compressed_size: required(
                total_compressed_size,
                "ColumnMetaData.total_compressed_size",
            )?,
            key_value_metadata,
            data_page_offset: required(data_page_offset, "ColumnMetaData.data_page_offset")?,
            index_page_offset,
            dictionary_page_offset,
            statistics,
        })
    }

    fn write(&self, w: &mut CompactWriter) {
        w.struct_begin();
        w.field_i32(1, self.type_.to_i32());
        w.field_list(2, ttype::I32, self.encodings.len());
        for e in &self.encodings {
            w.write_i32(e.to_i32());
        }
        write_string_list(w, 3, &self.path_in_schema);
        w.field_i32(4, self.codec.to_i32());
        w.field_i64(5, self.num_values);
        w.field_i64(6, self.total_uncompressed_size);
        w.field_i64(7, self.total_compressed_size);
        if let Some(v) = &self.key_value_metadata {
            write_struct_list(w, 8, v);
        }
        w.field_i64(9, self.data_page_offset);
        if let Some(v) = self.index_page_offset {
            w.field_i64(10, v);
        }
        if let Some(v) = self.dictionary_page_offset {
            w.field_i64(11, v);
        }
        if let Some(v) = &self.statistics {
            w.field_struct(12);
            v.write(w);
        }
        w.struct_end();
    }
}

impl ThriftStruct for ColumnChunk {
    fn read(r: &mut CompactReader<'_>) -> Result<Self> {
        let (mut file_path, mut file_offset, mut meta_data) = (None, 0, None);
        r.struct_begin()?;
        while let Some((id, t)) = r.field_header()? {
            match id {
                1 => file_path = Some(read_string_field(r, t, "ColumnChunk.file_path")?),
                2 => file_offset = read_i64_field(r, t, "ColumnChunk.file_offset")?,
                3 => meta_data = Some(read_struct_field(r, t, "ColumnChunk.meta_data")?),
                _ => r.skip(t)?,
            }
        }
        r.struct_end();
        Ok(ColumnChunk {
            file_path,
            file_offset,
            meta_data,
        })
    }

    fn write(&self, w: &mut CompactWriter) {
        w.struct_begin();
        if let Some(v) = &self.file_path {
            w.field_binary(1, v.as_bytes());
        }
        w.field_i64(2, self.file_offset);
        if let Some(v) = &self.meta_data {
            w.field_struct(3);
            v.write(w);
        }
        w.struct_end();
    }
}

impl ThriftStruct for RowGroup {
    fn read(r: &mut CompactReader<'_>) -> Result<Self> {
        let mut columns = None;
        let mut total_byte_size = None;
        let mut num_rows = None;
        let mut file_offset = None;
        let mut total_compressed_size = None;
        let mut ordinal = None;
        r.struct_begin()?;
        while let Some((id, t)) = r.field_header()? {
            match id {
                1 => {
                    columns = Some(read_list(
                        r,
                        t,
                        ttype::STRUCT,
                        "RowGroup.columns",
                        ColumnChunk::read,
                    )?)
                }
                2 => total_byte_size = Some(read_i64_field(r, t, "RowGroup.total_byte_size")?),
                3 => num_rows = Some(read_i64_field(r, t, "RowGroup.num_rows")?),
                5 => file_offset = Some(read_i64_field(r, t, "RowGroup.file_offset")?),
                6 => {
                    total_compressed_size =
                        Some(read_i64_field(r, t, "RowGroup.total_compressed_size")?)
                }
                7 => {
                    want(t, ttype::I16, "RowGroup.ordinal")?;
                    ordinal = Some(r.read_i16()?);
                }
                _ => r.skip(t)?,
            }
        }
        r.struct_end();
        Ok(RowGroup {
            columns: required(columns, "RowGroup.columns")?,
            total_byte_size: required(total_byte_size, "RowGroup.total_byte_size")?,
            num_rows: required(num_rows, "RowGroup.num_rows")?,
            file_offset,
            total_compressed_size,
            ordinal,
        })
    }

    fn write(&self, w: &mut CompactWriter) {
        w.struct_begin();
        write_struct_list(w, 1, &self.columns);
        w.field_i64(2, self.total_byte_size);
        w.field_i64(3, self.num_rows);
        if let Some(v) = self.file_offset {
            w.field_i64(5, v);
        }
        if let Some(v) = self.total_compressed_size {
            w.field_i64(6, v);
        }
        if let Some(v) = self.ordinal {
            w.field_i16(7, v);
        }
        w.struct_end();
    }
}

impl ThriftStruct for FileMetaData {
    fn read(r: &mut CompactReader<'_>) -> Result<Self> {
        let mut version = None;
        let mut schema = None;
        let mut num_rows = None;
        let mut row_groups = None;
        let mut key_value_metadata = None;
        let mut created_by = None;
        let mut column_orders = None;
        r.struct_begin()?;
        while let Some((id, t)) = r.field_header()? {
            match id {
                1 => version = Some(read_i32_field(r, t, "FileMetaData.version")?),
                2 => {
                    schema = Some(read_list(
                        r,
                        t,
                        ttype::STRUCT,
                        "FileMetaData.schema",
                        SchemaElement::read,
                    )?)
                }
                3 => num_rows = Some(read_i64_field(r, t, "FileMetaData.num_rows")?),
                4 => {
                    row_groups = Some(read_list(
                        r,
                        t,
                        ttype::STRUCT,
                        "FileMetaData.row_groups",
                        RowGroup::read,
                    )?)
                }
                5 => {
                    key_value_metadata = Some(read_list(
                        r,
                        t,
                        ttype::STRUCT,
                        "FileMetaData.key_value_metadata",
                        KeyValue::read,
                    )?)
                }
                6 => created_by = Some(read_string_field(r, t, "FileMetaData.created_by")?),
                7 => {
                    column_orders = Some(read_list(
                        r,
                        t,
                        ttype::STRUCT,
                        "FileMetaData.column_orders",
                        ColumnOrder::read,
                    )?)
                }
                _ => r.skip(t)?,
            }
        }
        r.struct_end();
        Ok(FileMetaData {
            version: required(version, "FileMetaData.version")?,
            schema: required(schema, "FileMetaData.schema")?,
            num_rows: required(num_rows, "FileMetaData.num_rows")?,
            row_groups: required(row_groups, "FileMetaData.row_groups")?,
            key_value_metadata,
            created_by,
            column_orders,
        })
    }

    fn write(&self, w: &mut CompactWriter) {
        w.struct_begin();
        w.field_i32(1, self.version);
        write_struct_list(w, 2, &self.schema);
        w.field_i64(3, self.num_rows);
        write_struct_list(w, 4, &self.row_groups);
        if let Some(v) = &self.key_value_metadata {
            write_struct_list(w, 5, v);
        }
        if let Some(v) = &self.created_by {
            w.field_binary(6, v.as_bytes());
        }
        if let Some(v) = &self.column_orders {
            write_struct_list(w, 7, v);
        }
        w.struct_end();
    }
}

impl ThriftStruct for DataPageHeader {
    fn read(r: &mut CompactReader<'_>) -> Result<Self> {
        let mut num_values = None;
        let mut encoding = None;
        let mut def = None;
        let mut rep = None;
        let mut statistics = None;
        r.struct_begin()?;
        while let Some((id, t)) = r.field_header()? {
            match id {
                1 => num_values = Some(read_i32_field(r, t, "DataPageHeader.num_values")?),
                2 => {
                    encoding = Some(Encoding::from_i32(read_i32_field(
                        r,
                        t,
                        "DataPageHeader.encoding",
                    )?))
                }
                3 => {
                    def = Some(Encoding::from_i32(read_i32_field(
                        r,
                        t,
                        "DataPageHeader.definition_level_encoding",
                    )?))
                }
                4 => {
                    rep = Some(Encoding::from_i32(read_i32_field(
                        r,
                        t,
                        "DataPageHeader.repetition_level_encoding",
                    )?))
                }
                5 => statistics = Some(read_struct_field(r, t, "DataPageHeader.statistics")?),
                _ => r.skip(t)?,
            }
        }
        r.struct_end();
        Ok(DataPageHeader {
            num_values: required(num_values, "DataPageHeader.num_values")?,
            encoding: required(encoding, "DataPageHeader.encoding")?,
            definition_level_encoding: required(def, "DataPageHeader.definition_level_encoding")?,
            repetition_level_encoding: required(rep, "DataPageHeader.repetition_level_encoding")?,
            statistics,
        })
    }

    fn write(&self, w: &mut CompactWriter) {
        w.struct_begin();
        w.field_i32(1, self.num_values);
        w.field_i32(2, self.encoding.to_i32());
        w.field_i32(3, self.definition_level_encoding.to_i32());
        w.field_i32(4, self.repetition_level_encoding.to_i32());
        if let Some(v) = &self.statistics {
            w.field_struct(5);
            v.write(w);
        }
        w.struct_end();
    }
}

impl ThriftStruct for DictionaryPageHeader {
    fn read(r: &mut CompactReader<'_>) -> Result<Self> {
        let (mut num_values, mut encoding, mut is_sorted) = (None, None, None);
        r.struct_begin()?;
        while let Some((id, t)) = r.field_header()? {
            match id {
                1 => num_values = Some(read_i32_field(r, t, "DictionaryPageHeader.num_values")?),
                2 => {
                    encoding = Some(Encoding::from_i32(read_i32_field(
                        r,
                        t,
                        "DictionaryPageHeader.encoding",
                    )?))
                }
                3 => is_sorted = Some(read_bool_field(r, t, "DictionaryPageHeader.is_sorted")?),
                _ => r.skip(t)?,
            }
        }
        r.struct_end();
        Ok(DictionaryPageHeader {
            num_values: required(num_values, "DictionaryPageHeader.num_values")?,
            encoding: required(encoding, "DictionaryPageHeader.encoding")?,
            is_sorted,
        })
    }

    fn write(&self, w: &mut CompactWriter) {
        w.struct_begin();
        w.field_i32(1, self.num_values);
        w.field_i32(2, self.encoding.to_i32());
        if let Some(v) = self.is_sorted {
            w.field_bool(3, v);
        }
        w.struct_end();
    }
}

impl ThriftStruct for DataPageHeaderV2 {
    fn read(r: &mut CompactReader<'_>) -> Result<Self> {
        let mut num_values = None;
        let mut num_nulls = None;
        let mut num_rows = None;
        let mut encoding = None;
        let mut def_len = None;
        let mut rep_len = None;
        let mut is_compressed = None;
        let mut statistics = None;
        r.struct_begin()?;
        while let Some((id, t)) = r.field_header()? {
            match id {
                1 => num_values = Some(read_i32_field(r, t, "DataPageHeaderV2.num_values")?),
                2 => num_nulls = Some(read_i32_field(r, t, "DataPageHeaderV2.num_nulls")?),
                3 => num_rows = Some(read_i32_field(r, t, "DataPageHeaderV2.num_rows")?),
                4 => {
                    encoding = Some(Encoding::from_i32(read_i32_field(
                        r,
                        t,
                        "DataPageHeaderV2.encoding",
                    )?))
                }
                5 => {
                    def_len = Some(read_i32_field(
                        r,
                        t,
                        "DataPageHeaderV2.definition_levels_byte_length",
                    )?)
                }
                6 => {
                    rep_len = Some(read_i32_field(
                        r,
                        t,
                        "DataPageHeaderV2.repetition_levels_byte_length",
                    )?)
                }
                7 => is_compressed = Some(read_bool_field(r, t, "DataPageHeaderV2.is_compressed")?),
                8 => statistics = Some(read_struct_field(r, t, "DataPageHeaderV2.statistics")?),
                _ => r.skip(t)?,
            }
        }
        r.struct_end();
        Ok(DataPageHeaderV2 {
            num_values: required(num_values, "DataPageHeaderV2.num_values")?,
            num_nulls: required(num_nulls, "DataPageHeaderV2.num_nulls")?,
            num_rows: required(num_rows, "DataPageHeaderV2.num_rows")?,
            encoding: required(encoding, "DataPageHeaderV2.encoding")?,
            definition_levels_byte_length: required(
                def_len,
                "DataPageHeaderV2.definition_levels_byte_length",
            )?,
            repetition_levels_byte_length: required(
                rep_len,
                "DataPageHeaderV2.repetition_levels_byte_length",
            )?,
            is_compressed,
            statistics,
        })
    }

    fn write(&self, w: &mut CompactWriter) {
        w.struct_begin();
        w.field_i32(1, self.num_values);
        w.field_i32(2, self.num_nulls);
        w.field_i32(3, self.num_rows);
        w.field_i32(4, self.encoding.to_i32());
        w.field_i32(5, self.definition_levels_byte_length);
        w.field_i32(6, self.repetition_levels_byte_length);
        if let Some(v) = self.is_compressed {
            w.field_bool(7, v);
        }
        if let Some(v) = &self.statistics {
            w.field_struct(8);
            v.write(w);
        }
        w.struct_end();
    }
}

impl ThriftStruct for PageHeader {
    fn read(r: &mut CompactReader<'_>) -> Result<Self> {
        let mut type_ = None;
        let mut uncompressed = None;
        let mut compressed = None;
        let mut crc = None;
        let mut data_page_header = None;
        let mut dictionary_page_header = None;
        let mut data_page_header_v2 = None;
        r.struct_begin()?;
        while let Some((id, t)) = r.field_header()? {
            match id {
                1 => type_ = Some(PageType::from_i32(read_i32_field(r, t, "PageHeader.type")?)),
                2 => {
                    uncompressed = Some(read_i32_field(r, t, "PageHeader.uncompressed_page_size")?)
                }
                3 => compressed = Some(read_i32_field(r, t, "PageHeader.compressed_page_size")?),
                4 => crc = Some(read_i32_field(r, t, "PageHeader.crc")?),
                5 => {
                    data_page_header = Some(read_struct_field(r, t, "PageHeader.data_page_header")?)
                }
                7 => {
                    dictionary_page_header = Some(read_struct_field(
                        r,
                        t,
                        "PageHeader.dictionary_page_header",
                    )?)
                }
                8 => {
                    data_page_header_v2 =
                        Some(read_struct_field(r, t, "PageHeader.data_page_header_v2")?)
                }
                _ => r.skip(t)?,
            }
        }
        r.struct_end();
        Ok(PageHeader {
            type_: required(type_, "PageHeader.type")?,
            uncompressed_page_size: required(uncompressed, "PageHeader.uncompressed_page_size")?,
            compressed_page_size: required(compressed, "PageHeader.compressed_page_size")?,
            crc,
            data_page_header,
            dictionary_page_header,
            data_page_header_v2,
        })
    }

    fn write(&self, w: &mut CompactWriter) {
        w.struct_begin();
        w.field_i32(1, self.type_.to_i32());
        w.field_i32(2, self.uncompressed_page_size);
        w.field_i32(3, self.compressed_page_size);
        if let Some(v) = self.crc {
            w.field_i32(4, v);
        }
        if let Some(v) = &self.data_page_header {
            w.field_struct(5);
            v.write(w);
        }
        if let Some(v) = &self.dictionary_page_header {
            w.field_struct(7);
            v.write(w);
        }
        if let Some(v) = &self.data_page_header_v2 {
            w.field_struct(8);
            v.write(w);
        }
        w.struct_end();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip<T: ThriftStruct + PartialEq + std::fmt::Debug>(v: &T) {
        let bytes = encode(v);
        let (back, used) = decode::<T>(&bytes).unwrap();
        assert_eq!(&back, v);
        assert_eq!(used, bytes.len());
    }

    fn full_stats() -> Statistics {
        Statistics {
            max: Some(vec![9, 0, 0, 0]),
            min: Some(vec![1, 0, 0, 0]),
            null_count: Some(3),
            distinct_count: Some(7),
            max_value: Some(b"zz".to_vec()),
            min_value: Some(Vec::new()),
            is_max_value_exact: Some(true),
            is_min_value_exact: Some(false),
        }
    }

    fn full_schema_element(logical_type: LogicalType) -> SchemaElement {
        SchemaElement {
            type_: Some(Type::FixedLenByteArray),
            type_length: Some(16),
            repetition_type: Some(FieldRepetitionType::Optional),
            name: "price".into(),
            num_children: Some(0),
            converted_type: Some(ConvertedType::Decimal),
            scale: Some(2),
            precision: Some(30),
            field_id: Some(-4),
            logical_type: Some(logical_type),
        }
    }

    fn min_schema_element() -> SchemaElement {
        SchemaElement {
            type_: None,
            type_length: None,
            repetition_type: None,
            name: "schema".into(),
            num_children: None,
            converted_type: None,
            scale: None,
            precision: None,
            field_id: None,
            logical_type: None,
        }
    }

    fn full_kv() -> KeyValue {
        KeyValue {
            key: "k".into(),
            value: Some("v".into()),
        }
    }

    fn full_column_meta() -> ColumnMetaData {
        ColumnMetaData {
            type_: Type::Int64,
            encodings: vec![
                Encoding::RleDictionary,
                Encoding::Plain,
                Encoding::Rle,
                Encoding::Unknown(99),
            ],
            path_in_schema: vec!["a".into(), "b".into()],
            codec: CompressionCodec::Snappy,
            num_values: 1_000_000,
            total_uncompressed_size: 1 << 33,
            total_compressed_size: 12345,
            key_value_metadata: Some(vec![full_kv()]),
            data_page_offset: 4,
            index_page_offset: Some(5),
            dictionary_page_offset: Some(6),
            statistics: Some(full_stats()),
        }
    }

    fn min_column_meta() -> ColumnMetaData {
        ColumnMetaData {
            type_: Type::Boolean,
            encodings: Vec::new(),
            path_in_schema: vec!["x".into()],
            codec: CompressionCodec::Uncompressed,
            num_values: 0,
            total_uncompressed_size: 0,
            total_compressed_size: 0,
            key_value_metadata: None,
            data_page_offset: 4,
            index_page_offset: None,
            dictionary_page_offset: None,
            statistics: None,
        }
    }

    fn full_chunk() -> ColumnChunk {
        ColumnChunk {
            file_path: Some("other.parquet".into()),
            file_offset: 77,
            meta_data: Some(full_column_meta()),
        }
    }

    fn min_chunk() -> ColumnChunk {
        ColumnChunk {
            file_path: None,
            file_offset: 0,
            meta_data: None,
        }
    }

    fn full_row_group() -> RowGroup {
        RowGroup {
            columns: vec![full_chunk(), min_chunk()],
            total_byte_size: 1 << 40,
            num_rows: 8192,
            file_offset: Some(4),
            total_compressed_size: Some(999),
            ordinal: Some(-2),
        }
    }

    fn min_row_group() -> RowGroup {
        RowGroup {
            columns: Vec::new(),
            total_byte_size: 0,
            num_rows: 0,
            file_offset: None,
            total_compressed_size: None,
            ordinal: None,
        }
    }

    #[test]
    fn enums_follow_parquet_thrift() {
        assert_eq!(Type::FixedLenByteArray.to_i32(), 7);
        assert_eq!(ConvertedType::from_i32(21), ConvertedType::Interval);
        assert_eq!(FieldRepetitionType::Repeated.to_i32(), 2);
        assert_eq!(Encoding::from_i32(1), Encoding::Unknown(1));
        assert_eq!(Encoding::RleDictionary.to_i32(), 8);
        assert_eq!(Encoding::ByteStreamSplit.name(), "BYTE_STREAM_SPLIT");
        assert_eq!(CompressionCodec::Lz4Raw.to_i32(), 7);
        assert_eq!(CompressionCodec::Zstd.name(), "ZSTD");
        assert_eq!(PageType::DataPageV2.to_i32(), 3);
        assert_eq!(PageType::from_i32(9).name(), "UNKNOWN(9)");
        for v in -1..25 {
            assert_eq!(ConvertedType::from_i32(v).to_i32(), v);
            assert_eq!(Encoding::from_i32(v).to_i32(), v);
        }
    }

    #[test]
    fn statistics_round_trip() {
        round_trip(&full_stats());
        round_trip(&Statistics::default());
    }

    #[test]
    fn schema_element_round_trip_with_every_logical_type() {
        let types = [
            LogicalType::String,
            LogicalType::Enum,
            LogicalType::Decimal {
                scale: 2,
                precision: 30,
            },
            LogicalType::Date,
            LogicalType::Timestamp {
                is_adjusted_to_utc: false,
                unit: TimeUnit::Millis,
            },
            LogicalType::Timestamp {
                is_adjusted_to_utc: true,
                unit: TimeUnit::Micros,
            },
            LogicalType::Timestamp {
                is_adjusted_to_utc: false,
                unit: TimeUnit::Nanos,
            },
            LogicalType::Integer {
                bit_width: 16,
                is_signed: true,
            },
            LogicalType::Integer {
                bit_width: 64,
                is_signed: false,
            },
            LogicalType::Unknown(12),
        ];
        for lt in types {
            round_trip(&lt);
            round_trip(&full_schema_element(lt));
        }
        round_trip(&min_schema_element());
    }

    #[test]
    fn key_value_round_trip() {
        round_trip(&full_kv());
        round_trip(&KeyValue {
            key: String::new(),
            value: None,
        });
    }

    #[test]
    fn column_meta_data_round_trip() {
        round_trip(&full_column_meta());
        round_trip(&min_column_meta());
        // A list of 15+ encodings uses the long list header.
        let mut many = min_column_meta();
        many.encodings = vec![Encoding::Plain; 20];
        many.path_in_schema = (0..16).map(|i| format!("p{i}")).collect();
        round_trip(&many);
    }

    #[test]
    fn column_chunk_and_row_group_round_trip() {
        round_trip(&full_chunk());
        round_trip(&min_chunk());
        round_trip(&full_row_group());
        round_trip(&min_row_group());
    }

    #[test]
    fn file_meta_data_round_trip() {
        let full = FileMetaData {
            version: 2,
            schema: vec![
                SchemaElement {
                    num_children: Some(2),
                    ..min_schema_element()
                },
                full_schema_element(LogicalType::Date),
                full_schema_element(LogicalType::String),
            ],
            num_rows: 8192,
            row_groups: vec![full_row_group(), min_row_group()],
            key_value_metadata: Some(vec![full_kv(), full_kv()]),
            created_by: Some("veta 0.1.0".into()),
            column_orders: Some(vec![ColumnOrder::TypeDefined, ColumnOrder::Other(2)]),
        };
        round_trip(&full);
        round_trip(&FileMetaData {
            version: 1,
            schema: Vec::new(),
            num_rows: 0,
            row_groups: Vec::new(),
            key_value_metadata: None,
            created_by: None,
            column_orders: None,
        });
    }

    #[test]
    fn page_headers_round_trip() {
        let v1 = DataPageHeader {
            num_values: 8192,
            encoding: Encoding::RleDictionary,
            definition_level_encoding: Encoding::Rle,
            repetition_level_encoding: Encoding::Rle,
            statistics: Some(full_stats()),
        };
        let v1_min = DataPageHeader {
            statistics: None,
            ..v1.clone()
        };
        let dict = DictionaryPageHeader {
            num_values: 10,
            encoding: Encoding::Plain,
            is_sorted: Some(false),
        };
        let dict_min = DictionaryPageHeader {
            is_sorted: None,
            ..dict.clone()
        };
        let v2 = DataPageHeaderV2 {
            num_values: 100,
            num_nulls: 3,
            num_rows: 100,
            encoding: Encoding::DeltaByteArray,
            definition_levels_byte_length: 12,
            repetition_levels_byte_length: 0,
            is_compressed: Some(false),
            statistics: Some(full_stats()),
        };
        let v2_min = DataPageHeaderV2 {
            is_compressed: None,
            statistics: None,
            ..v2.clone()
        };
        assert!(!v2.is_compressed());
        assert!(v2_min.is_compressed());
        round_trip(&v1);
        round_trip(&v1_min);
        round_trip(&dict);
        round_trip(&dict_min);
        round_trip(&v2);
        round_trip(&v2_min);
        let full = PageHeader {
            type_: PageType::DataPage,
            uncompressed_page_size: 1000,
            compressed_page_size: 900,
            crc: Some(-123_456_789),
            data_page_header: Some(v1),
            dictionary_page_header: Some(dict),
            data_page_header_v2: Some(v2),
        };
        round_trip(&full);
        round_trip(&PageHeader {
            type_: PageType::IndexPage,
            uncompressed_page_size: 0,
            compressed_page_size: 0,
            crc: None,
            data_page_header: None,
            dictionary_page_header: None,
            data_page_header_v2: None,
        });
    }

    #[test]
    fn page_header_decode_reports_its_length() {
        let header = PageHeader {
            type_: PageType::DictionaryPage,
            uncompressed_page_size: 40,
            compressed_page_size: 40,
            crc: None,
            data_page_header: None,
            dictionary_page_header: Some(DictionaryPageHeader {
                num_values: 10,
                encoding: Encoding::PlainDictionary,
                is_sorted: None,
            }),
            data_page_header_v2: None,
        };
        let mut bytes = encode(&header);
        let len = bytes.len();
        bytes.extend_from_slice(&[0xab; 40]); // the page data that follows
        let (back, used) = decode::<PageHeader>(&bytes).unwrap();
        assert_eq!(back, header);
        assert_eq!(used, len);
    }

    #[test]
    fn unknown_nested_fields_are_skipped() {
        let mut w = CompactWriter::new();
        w.struct_begin();
        w.field_list(1, ttype::STRUCT, 0); // columns
        w.field_i64(2, 10); // total_byte_size
        // 4: sorting_columns, a field the project skips: list<SortingColumn>.
        w.field_list(4, ttype::STRUCT, 2);
        for i in 0..2 {
            w.struct_begin();
            w.field_i32(1, i);
            w.field_bool(2, true);
            w.field_bool(3, false);
            w.struct_end();
        }
        // 50: unknown struct { 1: struct { 1: list<struct { 7: binary }>, 2: bool }, 3: double }
        w.field_struct(50);
        w.struct_begin();
        w.field_struct(1);
        w.struct_begin();
        w.field_list(1, ttype::STRUCT, 1);
        w.struct_begin();
        w.field_binary(7, b"deep");
        w.struct_end();
        w.field_bool(2, true);
        w.struct_end();
        w.field_double(3, 1.25);
        w.struct_end();
        // Known fields after the unknown ones; num_rows goes back to id 3 (long form).
        w.field_i64(3, 4096); // num_rows
        w.field_i64(6, 77); // total_compressed_size
        w.struct_end();
        let bytes = w.into_bytes();
        let (rg, used) = decode::<RowGroup>(&bytes).unwrap();
        assert_eq!(used, bytes.len());
        assert_eq!(
            rg,
            RowGroup {
                columns: Vec::new(),
                total_byte_size: 10,
                num_rows: 4096,
                file_offset: None,
                total_compressed_size: Some(77),
                ordinal: None,
            }
        );
    }

    #[test]
    fn unknown_logical_type_variant_is_kept_by_id() {
        // TIME (7) is not used by the project: { 7: { 1: true, 2: { 2: {} } } }
        let mut w = CompactWriter::new();
        w.struct_begin();
        w.field_struct(7);
        w.struct_begin();
        w.field_bool(1, true);
        w.field_struct(2);
        TimeUnit::Micros.write(&mut w);
        w.struct_end();
        w.struct_end();
        let (lt, _) = decode::<LogicalType>(&w.into_bytes()).unwrap();
        assert_eq!(lt, LogicalType::Unknown(7));
    }

    #[test]
    fn missing_required_field_fails() {
        let mut w = CompactWriter::new();
        w.struct_begin();
        w.field_binary(2, b"value only");
        w.struct_end();
        let err = decode::<KeyValue>(&w.into_bytes()).unwrap_err();
        assert!(err.to_string().contains("KeyValue.key"), "{err}");
    }

    #[test]
    fn wrong_field_type_fails() {
        let mut w = CompactWriter::new();
        w.struct_begin();
        w.field_i64(1, 2); // version must be i32
        w.struct_end();
        assert!(decode::<FileMetaData>(&w.into_bytes()).is_err());
    }

    #[test]
    fn union_with_two_fields_fails() {
        let mut w = CompactWriter::new();
        w.struct_begin();
        write_empty_struct(&mut w, 1);
        write_empty_struct(&mut w, 6);
        w.struct_end();
        assert!(decode::<LogicalType>(&w.into_bytes()).is_err());
    }
}
