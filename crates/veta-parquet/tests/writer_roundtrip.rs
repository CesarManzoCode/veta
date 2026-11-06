//! The writer against the reader (contract §9): every type × encoding × compression × page
//! version, with and without NULL and with several row groups; statistics against the bounds
//! computed here; the dictionary fallback; the types without a mapping.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use veta_core::rand::Rng;
use veta_core::{Chunk, DataType, Field, Schema, Value, Vector, VetaError};
use veta_parquet::meta::{
    ColumnMetaData, CompressionCodec, ConvertedType, Encoding, FieldRepetitionType, LogicalType,
    PageHeader, PageType, TimeUnit, Type, decode,
};
use veta_parquet::{
    Compression, PageVersion, ParquetFile, ParquetWriter, ValueEncoding, WriterOptions,
};

const DECIMAL: DataType = DataType::Decimal {
    precision: 12,
    scale: 2,
};
const TYPES: [DataType; 9] = [
    DataType::Boolean,
    DataType::Int16,
    DataType::Int32,
    DataType::Int64,
    DECIMAL,
    DataType::Date,
    DataType::Timestamp,
    DataType::Varchar,
    DataType::Float64,
];

struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn temp_file() -> TempFile {
    static N: AtomicU32 = AtomicU32::new(0);
    TempFile(std::env::temp_dir().join(format!(
        "veta-writer-{}-{}.parquet",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )))
}

/// Writes `rows` in chunks of varied sizes and opens the result.
fn write(schema: &Schema, rows: &Chunk, options: WriterOptions) -> (TempFile, ParquetFile) {
    let temp = temp_file();
    let file = fs::File::create(&temp.0).unwrap();
    let mut writer = ParquetWriter::new(file, schema, options).unwrap();
    let mut offset = 0;
    for &size in [1usize, 0, 4096, 8192, 3, 8192, 12_345].iter().cycle() {
        if offset == rows.len {
            break;
        }
        let take = size.min(rows.len - offset);
        writer.write_chunk(rows.slice(offset, take)).unwrap();
        offset += take;
    }
    writer.finish().unwrap();
    let opened = ParquetFile::open(&temp.0).unwrap();
    (temp, opened)
}

fn options(
    encoding: ValueEncoding,
    compression: Compression,
    page_version: PageVersion,
    row_group_rows: usize,
) -> WriterOptions {
    WriterOptions {
        encoding,
        compression,
        page_version,
        row_group_rows,
    }
}

fn value_of(data_type: DataType, rng: &mut Rng, i: usize) -> Value {
    match data_type {
        DataType::Boolean => Value::Boolean(rng.next_bool(0.3)),
        DataType::Int16 => Value::Int16(match i % 1000 {
            0 => i16::MIN,
            1 => i16::MAX,
            _ => rng.range_i64(-500, 500) as i16,
        }),
        DataType::Int32 => Value::Int32(match i % 1000 {
            0 => i32::MIN,
            1 => i32::MAX,
            _ => i as i32 + rng.range_i64(0, 9) as i32,
        }),
        DataType::Int64 => Value::Int64(match i % 1000 {
            0 => i64::MIN,
            1 => i64::MAX,
            _ => rng.range_i64(-1_000_000_000_000, 1_000_000_000_000),
        }),
        DataType::Decimal { precision, scale } => Value::Decimal {
            value: i128::from(rng.range_i64(-99_999_999_999, 99_999_999_999)),
            precision,
            scale,
        },
        DataType::Date => Value::Date(rng.range_i64(-20_000, 40_000) as i32),
        DataType::Timestamp => Value::Timestamp(rng.range_i64(-2_000_000_000, 4_000_000_000)),
        DataType::Varchar => match i % 50 {
            0 => Value::varchar(""),
            1 => Value::varchar([0xff, 0x00, b'\t', 0x80]),
            _ => Value::varchar(format!("https://example.com/{}", rng.below(300))),
        },
        DataType::Float64 => Value::Float64(match i % 1000 {
            0 => -0.0,
            1 => f64::INFINITY,
            2 => f64::NEG_INFINITY,
            _ => rng.next_f64() * 1e6 - 5e5,
        }),
        other => panic!("{other} has no parquet mapping"),
    }
}

/// Three columns per type: without NULL, with some NULL (and a run longer than a page) and
/// only NULL.
fn all_types(rows: usize, seed: u64) -> (Schema, Chunk) {
    let mut rng = Rng::new(seed);
    let mut fields = Vec::new();
    let mut columns = Vec::new();
    for (t, &data_type) in TYPES.iter().enumerate() {
        let full: Vec<Value> = (0..rows)
            .map(|i| value_of(data_type, &mut rng, i))
            .collect();
        let some: Vec<Value> = (0..rows)
            .map(|i| {
                if rng.next_bool(0.1) || (8192..8192 * 2).contains(&i) {
                    Value::Null
                } else {
                    value_of(data_type, &mut rng, i)
                }
            })
            .collect();
        let none = vec![Value::Null; rows];
        fields.push(Field::new(format!("full_{t}"), data_type, false));
        fields.push(Field::new(format!("some_{t}"), data_type, true));
        fields.push(Field::new(format!("none_{t}"), data_type, true));
        for values in [full, some, none] {
            columns.push(Vector::from_values(data_type, &values).unwrap());
        }
    }
    (Schema::new(fields), Chunk::new(columns).unwrap())
}

/// The same value in the same variant; doubles to the bit.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float64(x), Value::Float64(y)) => x.to_bits() == y.to_bits(),
        _ => std::mem::discriminant(a) == std::mem::discriminant(b) && a == b,
    }
}

fn assert_rows(file: &ParquetFile, expected: &Chunk) {
    let columns: Vec<usize> = (0..expected.num_columns()).collect();
    let mut row = 0;
    for chunk in file.chunks(&columns).unwrap() {
        let chunk = chunk.unwrap();
        for (c, column) in chunk.columns.iter().enumerate() {
            for i in 0..chunk.len {
                let (got, want) = (column.get(i), expected.column(c).get(row + i));
                assert!(
                    same(&got, &want),
                    "row {} column {c}: {got:?} {want:?}",
                    row + i
                );
            }
        }
        row += chunk.len;
    }
    assert_eq!(row, expected.len);
    assert_eq!(file.num_rows(), expected.len as u64);
}

fn chunk_meta(file: &ParquetFile, group: usize, column: usize) -> &ColumnMetaData {
    file.metadata().row_groups[group].columns[column]
        .meta_data
        .as_ref()
        .unwrap()
}

/// The statistics of every chunk against the bounds of its rows.
fn assert_statistics(file: &ParquetFile, expected: &Chunk) {
    let mut start = 0;
    for group in 0..file.num_row_groups() {
        let rows = file.row_group_rows(group);
        for c in 0..expected.num_columns() {
            let mut min: Option<Value> = None;
            let mut max: Option<Value> = None;
            let mut nulls = 0;
            for row in start..start + rows {
                let value = expected.column(c).get(row);
                if value.is_null() {
                    nulls += 1;
                    continue;
                }
                if min.as_ref().is_none_or(|m| value < *m) {
                    min = Some(value.clone());
                }
                if max.as_ref().is_none_or(|m| value > *m) {
                    max = Some(value);
                }
            }
            let stats = chunk_meta(file, group, c).statistics.as_ref().unwrap();
            assert_eq!(stats.null_count, Some(nulls), "group {group} column {c}");
            assert_eq!(stats.min, None);
            assert_eq!(stats.max, None);
            match (min, max) {
                (Some(min), Some(max)) => {
                    let got = file.row_group_stats(group, c).unwrap();
                    assert_eq!(got.0, min, "group {group} column {c}");
                    assert_eq!(got.1, max, "group {group} column {c}");
                    assert_eq!(got.2, nulls as u64);
                    assert_eq!(stats.is_min_value_exact, Some(true));
                    assert_eq!(stats.is_max_value_exact, Some(true));
                }
                _ => {
                    assert_eq!(stats.min_value, None);
                    assert_eq!(stats.max_value, None);
                    assert!(file.chunk_bounds(group, c).is_none());
                }
            }
        }
        start += rows;
    }
}

fn expected_encoding(encoding: ValueEncoding, data_type: DataType, has_values: bool) -> Encoding {
    let is_delta_int = matches!(
        data_type,
        DataType::Int16
            | DataType::Int32
            | DataType::Int64
            | DataType::Date
            | DataType::Decimal { .. }
    );
    match encoding {
        ValueEncoding::Plain => Encoding::Plain,
        ValueEncoding::Dict if has_values => Encoding::RleDictionary,
        ValueEncoding::Dict => Encoding::Plain,
        ValueEncoding::Delta if is_delta_int => Encoding::DeltaBinaryPacked,
        ValueEncoding::Delta if data_type == DataType::Varchar => Encoding::DeltaLengthByteArray,
        ValueEncoding::DeltaByteArray if is_delta_int => Encoding::DeltaBinaryPacked,
        ValueEncoding::DeltaByteArray if data_type == DataType::Varchar => Encoding::DeltaByteArray,
        _ => Encoding::Plain,
    }
}

/// Walks the pages of every chunk: sizes, offsets, kinds and rows per page.
fn assert_pages(path: &Path, file: &ParquetFile, options: WriterOptions) {
    let bytes = fs::read(path).unwrap();
    let schema = file.schema().clone();
    for group in 0..file.num_row_groups() {
        let rows = file.row_group_rows(group);
        let mut group_uncompressed = 0;
        let mut group_compressed = 0;
        let first = chunk_meta(file, group, 0);
        assert_eq!(
            file.metadata().row_groups[group].file_offset,
            Some(
                first
                    .dictionary_page_offset
                    .unwrap_or(first.data_page_offset)
            )
        );
        for c in 0..schema.len() {
            let md = chunk_meta(file, group, c);
            let field = schema.field(c);
            assert_eq!(md.num_values, rows as i64);
            assert_eq!(md.path_in_schema, [field.name.clone()]);
            assert_eq!(
                md.codec,
                match options.compression {
                    Compression::None => CompressionCodec::Uncompressed,
                    Compression::Snappy => CompressionCodec::Snappy,
                }
            );
            let has_values = md.statistics.as_ref().unwrap().null_count != Some(rows as i64);
            let data_encoding = expected_encoding(options.encoding, field.data_type, has_values);
            let mut encodings = vec![data_encoding];
            if data_encoding == Encoding::RleDictionary {
                encodings.push(Encoding::Plain);
            }
            if field.nullable {
                encodings.push(Encoding::Rle);
            }
            encodings.sort_by_key(|e| e.to_i32());
            assert_eq!(md.encodings, encodings, "{}", field.name);

            let start = md.dictionary_page_offset.unwrap_or(md.data_page_offset) as usize;
            let end = start + md.total_compressed_size as usize;
            let mut pos = start;
            let mut values = 0;
            let mut uncompressed = 0;
            while pos < end {
                let (header, used) = decode::<PageHeader>(&bytes[pos..end]).unwrap();
                if header.type_ == PageType::DictionaryPage {
                    assert_eq!(pos as i64, md.dictionary_page_offset.unwrap());
                    assert_eq!(
                        header.dictionary_page_header.as_ref().unwrap().encoding,
                        Encoding::Plain
                    );
                } else {
                    if values == 0 {
                        assert_eq!(pos as i64, md.data_page_offset);
                    }
                    let (count, encoding) = match options.page_version {
                        PageVersion::V1 => {
                            assert_eq!(header.type_, PageType::DataPage);
                            let h = header.data_page_header.as_ref().unwrap();
                            assert_eq!(h.definition_level_encoding, Encoding::Rle);
                            (h.num_values, h.encoding)
                        }
                        PageVersion::V2 => {
                            assert_eq!(header.type_, PageType::DataPageV2);
                            let h = header.data_page_header_v2.as_ref().unwrap();
                            assert_eq!(h.num_rows, h.num_values);
                            assert_eq!(h.repetition_levels_byte_length, 0);
                            assert_eq!(h.definition_levels_byte_length == 0, !field.nullable);
                            (h.num_values, h.encoding)
                        }
                    };
                    assert_eq!(encoding, data_encoding);
                    assert!((1..=8192).contains(&count));
                    // Every page but the last of a chunk is full.
                    assert!(count == 8192 || values + count == rows as i32);
                    values += count;
                }
                uncompressed += used as i64 + i64::from(header.uncompressed_page_size);
                pos += used + header.compressed_page_size as usize;
            }
            assert_eq!(pos, end);
            assert_eq!(values, rows as i32);
            assert_eq!(uncompressed, md.total_uncompressed_size);
            group_uncompressed += md.total_uncompressed_size;
            group_compressed += md.total_compressed_size;
        }
        let rg = &file.metadata().row_groups[group];
        assert_eq!(rg.total_byte_size, group_uncompressed);
        assert_eq!(rg.total_compressed_size, Some(group_compressed));
        assert_eq!(rg.ordinal, Some(group as i16));
    }
}

#[test]
fn every_type_encoding_compression_and_page_version_round_trips() {
    let (schema, rows) = all_types(20_000, 42);
    for encoding in ValueEncoding::ALL {
        for compression in [Compression::None, Compression::Snappy] {
            for page_version in [PageVersion::V1, PageVersion::V2] {
                let opts = options(encoding, compression, page_version, 9_000);
                let (temp, file) = write(&schema, &rows, opts);
                assert_eq!(file.schema(), &schema);
                assert_eq!(file.num_row_groups(), 3);
                let group_rows: Vec<usize> = (0..3).map(|g| file.row_group_rows(g)).collect();
                assert_eq!(group_rows, [9_000, 9_000, 2_000]);
                assert_rows(&file, &rows);
                assert_statistics(&file, &rows);
                assert_pages(&temp.0, &file, opts);
            }
        }
    }
}

#[test]
fn a_page_of_only_nulls_inside_a_chunk_with_values_round_trips() {
    let values: Vec<Value> = (0..8192)
        .map(|i| Value::varchar(format!("v{}", i % 5)))
        .chain(std::iter::repeat_n(Value::Null, 100))
        .collect();
    let schema = Schema::new(vec![Field::new("s", DataType::Varchar, true)]);
    let rows = Chunk::new(vec![
        Vector::from_values(DataType::Varchar, &values).unwrap(),
    ])
    .unwrap();
    for encoding in ValueEncoding::ALL {
        for compression in [Compression::None, Compression::Snappy] {
            for page_version in [PageVersion::V1, PageVersion::V2] {
                let (_temp, file) = write(
                    &schema,
                    &rows,
                    options(encoding, compression, page_version, 1_000_000),
                );
                assert_rows(&file, &rows);
            }
        }
    }
}

#[test]
fn row_groups_split_exactly_at_their_limit() {
    let schema = Schema::new(vec![Field::new("x", DataType::Int64, false)]);
    let values: Vec<Value> = (0..30).map(Value::Int64).collect();
    let rows = Chunk::new(vec![Vector::from_values(DataType::Int64, &values).unwrap()]).unwrap();
    for (limit, groups) in [(1, 30), (7, 5), (10, 3), (30, 1), (31, 1)] {
        let opts = options(
            ValueEncoding::Plain,
            Compression::None,
            PageVersion::V1,
            limit,
        );
        let (_temp, file) = write(&schema, &rows, opts);
        assert_eq!(file.num_row_groups(), groups, "limit {limit}");
        for g in 0..groups {
            assert!(file.row_group_rows(g) > 0 && file.row_group_rows(g) <= limit);
        }
        assert_rows(&file, &rows);
        assert_statistics(&file, &rows);
    }
}

#[test]
fn a_file_without_rows_has_a_schema_and_no_row_groups() {
    let schema = Schema::new(vec![
        Field::new("x", DataType::Int64, false),
        Field::new("s", DataType::Varchar, true),
    ]);
    let temp = temp_file();
    let writer = ParquetWriter::new(
        fs::File::create(&temp.0).unwrap(),
        &schema,
        WriterOptions::default(),
    )
    .unwrap();
    writer.finish().unwrap();
    let file = ParquetFile::open(&temp.0).unwrap();
    assert_eq!(file.schema(), &schema);
    assert_eq!(file.num_row_groups(), 0);
    assert_eq!(file.num_rows(), 0);
}

fn dict_options(row_group_rows: usize) -> WriterOptions {
    options(
        ValueEncoding::Dict,
        Compression::Snappy,
        PageVersion::V1,
        row_group_rows,
    )
}

#[test]
fn a_dictionary_past_65536_entries_falls_back_to_plain_by_chunk() {
    // Row groups of 65 537 rows: the first two have that many distinct values, the third few.
    let mut numbers: Vec<Value> = (0..65_537i64 * 2).map(Value::Int64).collect();
    numbers.extend((0..65_537i64).map(|i| Value::Int64(i % 3)));
    let strings: Vec<Value> = numbers
        .iter()
        .map(|v| Value::varchar(format!("{v:?}")))
        .collect();
    let schema = Schema::new(vec![
        Field::new("x", DataType::Int64, false),
        Field::new("s", DataType::Varchar, false),
    ]);
    let rows = Chunk::new(vec![
        Vector::from_values(DataType::Int64, &numbers).unwrap(),
        Vector::from_values(DataType::Varchar, &strings).unwrap(),
    ])
    .unwrap();
    let (_temp, file) = write(&schema, &rows, dict_options(65_537));
    assert_eq!(file.num_row_groups(), 3);
    assert_rows(&file, &rows);
    assert_statistics(&file, &rows);
    for c in 0..2 {
        for group in 0..2 {
            let md = chunk_meta(&file, group, c);
            assert_eq!(md.encodings, [Encoding::Plain], "column {c}");
            assert_eq!(md.dictionary_page_offset, None);
        }
        let md = chunk_meta(&file, 2, c);
        assert_eq!(md.encodings, [Encoding::Plain, Encoding::RleDictionary]);
        assert!(md.dictionary_page_offset.is_some());
    }
    // Exactly 65 536 entries still fit.
    let (_temp, exact) = write(&schema, &rows.slice(0, 65_536), dict_options(1_000_000));
    for c in 0..2 {
        let md = chunk_meta(&exact, 0, c);
        assert_eq!(md.encodings, [Encoding::Plain, Encoding::RleDictionary]);
        assert!(md.dictionary_page_offset.is_some());
    }
    assert_rows(&exact, &rows.slice(0, 65_536));
}

#[test]
fn a_dictionary_of_one_entry_takes_no_bits() {
    let values = vec![Value::varchar("same"); 10_000];
    let schema = Schema::new(vec![Field::new("s", DataType::Varchar, false)]);
    let rows = Chunk::new(vec![
        Vector::from_values(DataType::Varchar, &values).unwrap(),
    ])
    .unwrap();
    for page_version in [PageVersion::V1, PageVersion::V2] {
        let (_temp, file) = write(
            &schema,
            &rows,
            options(
                ValueEncoding::Dict,
                Compression::None,
                page_version,
                1_000_000,
            ),
        );
        assert_rows(&file, &rows);
    }
}

fn double_file(values: &[Value]) -> (Schema, Chunk) {
    let schema = Schema::new(vec![Field::new("d", DataType::Float64, true)]);
    let rows = Chunk::new(vec![
        Vector::from_values(DataType::Float64, values).unwrap(),
    ])
    .unwrap();
    (schema, rows)
}

#[test]
fn a_chunk_with_a_nan_has_no_bounds_and_keeps_its_rows() {
    let (schema, rows) = double_file(&[
        Value::Float64(1.0),
        Value::Float64(f64::NAN),
        Value::Null,
        Value::Float64(-2.0),
    ]);
    let (_temp, file) = write(&schema, &rows, WriterOptions::default());
    assert_rows(&file, &rows);
    let stats = chunk_meta(&file, 0, 0).statistics.as_ref().unwrap();
    assert_eq!(stats.min_value, None);
    assert_eq!(stats.max_value, None);
    assert_eq!(stats.null_count, Some(1));
    assert!(file.row_group_stats(0, 0).is_none());
}

#[test]
fn bounds_of_doubles_keep_the_sign_of_zero() {
    let (schema, rows) = double_file(&[Value::Float64(0.0), Value::Float64(-0.0)]);
    let (_temp, file) = write(&schema, &rows, WriterOptions::default());
    let stats = chunk_meta(&file, 0, 0).statistics.as_ref().unwrap();
    assert_eq!(stats.min_value, Some((-0.0f64).to_le_bytes().to_vec()));
    assert_eq!(stats.max_value, Some(0.0f64.to_le_bytes().to_vec()));
}

#[test]
fn statistics_are_not_truncated() {
    let long = "x".repeat(100_000);
    let schema = Schema::new(vec![Field::new("s", DataType::Varchar, false)]);
    let values = [Value::varchar(&long), Value::varchar(format!("{long}y"))];
    let rows = Chunk::new(vec![
        Vector::from_values(DataType::Varchar, &values).unwrap(),
    ])
    .unwrap();
    let (_temp, file) = write(&schema, &rows, WriterOptions::default());
    let (min, max, _) = file.row_group_stats(0, 0).unwrap();
    assert_eq!(min, values[0]);
    assert_eq!(max, values[1]);
}

#[test]
fn the_schema_follows_the_mapping_of_the_contract() {
    let (schema, rows) = all_types(10, 7);
    let (_temp, file) = write(&schema, &rows, WriterOptions::default());
    let meta = file.metadata();
    assert_eq!(
        meta.created_by.as_deref(),
        Some(concat!("veta ", env!("CARGO_PKG_VERSION")))
    );
    assert_eq!(meta.column_orders.as_ref().unwrap().len(), schema.len());
    assert_eq!(meta.schema[0].num_children, Some(schema.len() as i32));
    for (element, field) in meta.schema[1..].iter().zip(&schema.fields) {
        assert_eq!(element.name, field.name);
        assert_eq!(
            element.repetition_type,
            Some(if field.nullable {
                FieldRepetitionType::Optional
            } else {
                FieldRepetitionType::Required
            })
        );
        let (physical, logical) = match field.data_type {
            DataType::Boolean => (Type::Boolean, None),
            DataType::Int16 => (
                Type::Int32,
                Some(LogicalType::Integer {
                    bit_width: 16,
                    is_signed: true,
                }),
            ),
            DataType::Int32 => (Type::Int32, None),
            DataType::Int64 => (Type::Int64, None),
            DataType::Decimal { precision, scale } => (
                Type::Int64,
                Some(LogicalType::Decimal {
                    scale: i32::from(scale),
                    precision: i32::from(precision),
                }),
            ),
            DataType::Date => (Type::Int32, Some(LogicalType::Date)),
            DataType::Timestamp => (
                Type::Int64,
                Some(LogicalType::Timestamp {
                    is_adjusted_to_utc: false,
                    unit: TimeUnit::Millis,
                }),
            ),
            DataType::Varchar => (Type::ByteArray, Some(LogicalType::String)),
            DataType::Float64 => (Type::Double, None),
            other => panic!("{other}"),
        };
        assert_eq!(element.type_, Some(physical), "{}", field.name);
        assert_eq!(element.logical_type, logical, "{}", field.name);
        // TIMESTAMP_MILLIS would say adjusted to UTC.
        if field.data_type == DataType::Timestamp {
            assert_eq!(element.converted_type, None);
        }
        if field.data_type == DataType::Int16 {
            assert_eq!(element.converted_type, Some(ConvertedType::Int16));
        }
    }
}

fn new_writer_error(schema: &Schema, options: WriterOptions) -> VetaError {
    match ParquetWriter::new(Vec::new(), schema, options) {
        Ok(_) => panic!("expected an error"),
        Err(err) => err,
    }
}

#[test]
fn int128_and_wide_decimals_are_errors() {
    let wide = DataType::Decimal {
        precision: 19,
        scale: 2,
    };
    for data_type in [DataType::Int128, wide] {
        let schema = Schema::new(vec![Field::new("x", data_type, true)]);
        let err = new_writer_error(&schema, WriterOptions::default());
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().starts_with("unsupported: "), "{err}");
    }
    // The widest decimal that fits is fine.
    let schema = Schema::new(vec![Field::new(
        "x",
        DataType::Decimal {
            precision: 18,
            scale: 2,
        },
        true,
    )]);
    assert!(ParquetWriter::new(Vec::new(), &schema, WriterOptions::default()).is_ok());
}

#[test]
fn bad_writer_setups_are_errors() {
    let field = Field::new("x", DataType::Int64, false);
    let no_rows = WriterOptions {
        row_group_rows: 0,
        ..WriterOptions::default()
    };
    let schema = Schema::new(vec![field.clone()]);
    assert_eq!(new_writer_error(&schema, no_rows).exit_code(), 1);
    let twice = Schema::new(vec![field.clone(), field]);
    assert_eq!(
        new_writer_error(&twice, WriterOptions::default()).exit_code(),
        1
    );
}

#[test]
fn rows_that_do_not_fit_their_column_are_errors() {
    let run = |schema: &Schema, column: Vector| -> VetaError {
        let opts = options(ValueEncoding::Plain, Compression::None, PageVersion::V1, 1);
        let mut writer = ParquetWriter::new(Vec::new(), schema, opts).unwrap();
        writer
            .write_chunk(Chunk::new(vec![column]).unwrap())
            .unwrap_err()
    };
    // Seconds that overflow milliseconds.
    let schema = Schema::new(vec![Field::new("t", DataType::Timestamp, false)]);
    let big = Vector::from_values(DataType::Timestamp, &[Value::Timestamp(i64::MAX)]).unwrap();
    assert_eq!(run(&schema, big).exit_code(), 1);
    // A NULL in a column the schema says has none.
    let schema = Schema::new(vec![Field::new("x", DataType::Int64, false)]);
    let null = Vector::from_values(DataType::Int64, &[Value::Null]).unwrap();
    assert_eq!(run(&schema, null).exit_code(), 2);
    // A vector of another type.
    let other = Vector::from_values(DataType::Int32, &[Value::Int32(1)]).unwrap();
    assert_eq!(run(&schema, other).exit_code(), 2);
}
