//! Blocks (contract §5.3): the unit of every column file, of `primary.idx` and of
//! `sample.bin`.
//!
//! ```text
//! u32 payload_len | u32 rows | u8 encoding | u8 compressor | u8 flags | u8 reserved | u32 crc32c
//! payload (payload_len bytes)
//! ```
//!
//! The uncompressed payload is the validity map (only when `flags & 1`, bits LSB-first in
//! `ceil(rows/8)` bytes) followed by the output of the encoding `veta-codec` chose (§8.1).
//! The map is written only when the vector has at least one NULL. If `vlz` saves at least
//! 5 % of that payload (§8.2), the whole payload, map included, is stored compressed and
//! `compressor = 1`; its first field is then the decompressed length. The CRC32C covers
//! the 12 header bytes before it and the stored payload.
//!
//! Reading checks everything and reports any failure as [`VetaError::Corrupt`] naming the
//! part: truncated header or payload, CRC, unknown encoding/compressor, flags or reserved
//! bits, a row count other than the expected one, a validity map in a NOT NULL column and
//! any decoding error (including trailing bytes).

use std::borrow::Cow;

use veta_codec::{Encoding, decode, encode, vlz};
use veta_core::crc::{crc32c, crc32c_append};
use veta_core::{Bitmap, DataType, Result, Vector, VetaError};

/// Bytes of the block header.
pub const HEADER_LEN: usize = 16;
/// `compressor` byte: payload stored as is.
pub const COMPRESSOR_NONE: u8 = 0;
/// `compressor` byte: payload compressed with `vlz`.
pub const COMPRESSOR_VLZ: u8 = 1;
/// `flags` bit: the payload starts with the validity map.
pub const FLAG_VALIDITY: u8 = 1;

/// Parsed block header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockHeader {
    pub payload_len: u32,
    pub rows: u32,
    pub encoding: u8,
    pub compressor: u8,
    pub flags: u8,
    pub reserved: u8,
    pub crc: u32,
}

impl BlockHeader {
    /// Reads the header at the start of `bytes`; `Err` with the reason if it is truncated.
    pub fn parse(bytes: &[u8]) -> std::result::Result<BlockHeader, String> {
        if bytes.len() < HEADER_LEN {
            return Err(format!(
                "truncated block header: {} of {HEADER_LEN} bytes",
                bytes.len()
            ));
        }
        let u32_at = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4"));
        Ok(BlockHeader {
            payload_len: u32_at(0),
            rows: u32_at(4),
            encoding: bytes[8],
            compressor: bytes[9],
            flags: bytes[10],
            reserved: bytes[11],
            crc: u32_at(12),
        })
    }

    /// Header plus payload.
    pub fn block_len(&self) -> usize {
        HEADER_LEN + self.payload_len as usize
    }
}

/// Appends the block of `vector` to `out`.
pub fn write_block(vector: &Vector, out: &mut Vec<u8>) -> Result<()> {
    let rows = u32::try_from(vector.len())
        .map_err(|_| VetaError::internal("block has more than u32::MAX rows"))?;
    let (encoding, encoded) = encode(vector);
    let validity = vector.validity.as_ref().filter(|b| !b.all_set());
    let raw = match validity {
        Some(bitmap) => {
            let mut raw = Vec::with_capacity(bitmap.as_bytes().len() + encoded.len());
            raw.extend_from_slice(bitmap.as_bytes());
            raw.extend_from_slice(&encoded);
            raw
        }
        None => encoded,
    };
    if u32::try_from(raw.len()).is_err() {
        return Err(VetaError::internal("block payload exceeds 4 GiB"));
    }
    let compressed = vlz::compress(&raw);
    let (compressor, payload) = if vlz::pays_off(raw.len(), compressed.len()) {
        (COMPRESSOR_VLZ, compressed)
    } else {
        (COMPRESSOR_NONE, raw)
    };
    let flags = if validity.is_some() { FLAG_VALIDITY } else { 0 };
    let start = out.len();
    out.reserve(HEADER_LEN + payload.len());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&rows.to_le_bytes());
    out.extend_from_slice(&[encoding.id(), compressor, flags, 0]);
    let crc = crc32c_append(crc32c(&out[start..start + 12]), &payload);
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&payload);
    Ok(())
}

/// Encodes `vector` as a standalone block.
pub fn block_bytes(vector: &Vector) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    write_block(vector, &mut out)?;
    Ok(out)
}

/// Verifies and decodes the block at the start of `bytes`, which must hold
/// `expected_rows` values of `data_type`. Returns the vector (with a validity map only if
/// some row is NULL) and the length of the block. Every failure is `Corrupt` for `part`.
pub fn read_block(
    bytes: &[u8],
    data_type: DataType,
    nullable: bool,
    expected_rows: usize,
    part: &str,
) -> Result<(Vector, usize)> {
    let corrupt = |reason: String| VetaError::corrupt(part, reason);
    let header = BlockHeader::parse(bytes).map_err(corrupt)?;
    let len = header.block_len();
    if bytes.len() < len {
        return Err(corrupt(format!(
            "block payload truncated: {} of {} bytes",
            bytes.len() - HEADER_LEN,
            header.payload_len
        )));
    }
    let payload = &bytes[HEADER_LEN..len];
    let crc = crc32c_append(crc32c(&bytes[..12]), payload);
    if crc != header.crc {
        return Err(corrupt(format!(
            "block crc32c {crc:08x} does not match header {:08x}",
            header.crc
        )));
    }
    if header.rows as usize != expected_rows {
        return Err(corrupt(format!(
            "block has {} rows, expected {expected_rows}",
            header.rows
        )));
    }
    if header.reserved != 0 || header.flags & !FLAG_VALIDITY != 0 {
        return Err(corrupt(format!(
            "invalid block flags {:#04x} / reserved {:#04x}",
            header.flags, header.reserved
        )));
    }
    let has_validity = header.flags & FLAG_VALIDITY != 0;
    if has_validity && !nullable {
        return Err(corrupt("validity map in a NOT NULL column".into()));
    }
    let encoding = Encoding::from_id(header.encoding)
        .map_err(|_| corrupt(format!("unknown encoding id {}", header.encoding)))?;
    if !encoding.applies_to(&data_type) {
        return Err(corrupt(format!(
            "encoding {} is not valid for {data_type}",
            encoding.name()
        )));
    }
    let raw: Cow<'_, [u8]> = match header.compressor {
        COMPRESSOR_NONE => Cow::Borrowed(payload),
        COMPRESSOR_VLZ => Cow::Owned(
            vlz::decompress(payload).map_err(|e| corrupt(format!("vlz: {}", reason_of(&e))))?,
        ),
        other => return Err(corrupt(format!("unknown compressor {other}"))),
    };
    let rows = expected_rows;
    let (validity, encoded) = if has_validity {
        let map_len = rows.div_ceil(8);
        if raw.len() < map_len {
            return Err(corrupt("validity map truncated".into()));
        }
        let bitmap = Bitmap::from_bytes(&raw[..map_len], rows)
            .ok_or_else(|| corrupt("validity map truncated".into()))?;
        (Some(bitmap), &raw[map_len..])
    } else {
        (None, &raw[..])
    };
    let decoded = decode(encoding, encoded, &data_type, rows)
        .map_err(|e| corrupt(format!("{}: {}", encoding.name(), reason_of(&e))))?;
    let validity = validity.filter(|b| !b.all_set());
    let vector = Vector::new(decoded.data, validity).map_err(|e| corrupt(reason_of(&e)))?;
    Ok((vector, len))
}

/// Reads a block that must fill `bytes` exactly (a granule delimited by its marks).
pub fn read_block_exact(
    bytes: &[u8],
    data_type: DataType,
    nullable: bool,
    expected_rows: usize,
    part: &str,
) -> Result<Vector> {
    let (vector, len) = read_block(bytes, data_type, nullable, expected_rows, part)?;
    if len != bytes.len() {
        return Err(VetaError::corrupt(
            part,
            format!(
                "block of {len} bytes does not reach its next mark ({} bytes)",
                bytes.len()
            ),
        ));
    }
    Ok(vector)
}

/// The reason of a codec error without the empty part name the codec puts in it.
fn reason_of(err: &VetaError) -> String {
    match err {
        VetaError::Corrupt { reason, .. } => reason.clone(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use veta_core::Value;
    use veta_core::rand::Rng;

    fn assert_same(a: &Vector, b: &Vector) {
        assert_eq!(a.len(), b.len());
        assert_eq!(a.data_type(), b.data_type());
        for i in 0..a.len() {
            assert_eq!(a.get(i), b.get(i), "row {i}");
        }
    }

    fn types() -> Vec<DataType> {
        vec![
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
        ]
    }

    fn random_value(rng: &mut Rng, data_type: DataType) -> Value {
        match data_type {
            DataType::Boolean => Value::Boolean(rng.next_bool(0.5)),
            DataType::Int16 => Value::Int16(rng.range_i64(-300, 300) as i16),
            DataType::Int32 => Value::Int32(rng.range_i64(-100_000, 100_000) as i32),
            DataType::Int64 => Value::Int64(rng.next_u64() as i64 >> rng.below(60)),
            DataType::Int128 => Value::Int128(rng.next_u64() as i128),
            DataType::Float64 => Value::Float64(rng.next_f64() * 1e6 - 5e5),
            DataType::Decimal { precision, scale } => Value::Decimal {
                value: rng.range_i64(-1_000_000_000, 1_000_000_000) as i128,
                precision,
                scale,
            },
            DataType::Varchar => {
                let len = rng.below(20) as usize;
                Value::Varchar((0..len).map(|_| b'a' + rng.below(5) as u8).collect())
            }
            DataType::Date => Value::Date(rng.range_i64(-1000, 20_000) as i32),
            DataType::Timestamp => Value::Timestamp(rng.range_i64(0, 2_000_000_000)),
        }
    }

    fn random_vector(rng: &mut Rng, data_type: DataType, rows: usize, nulls: bool) -> Vector {
        let values: Vec<Value> = (0..rows)
            .map(|_| {
                if nulls && rng.next_bool(0.2) {
                    Value::Null
                } else {
                    random_value(rng, data_type)
                }
            })
            .collect();
        Vector::from_values(data_type, &values).unwrap()
    }

    #[test]
    fn round_trips_every_type_with_and_without_nulls() {
        let mut rng = Rng::new(11);
        for data_type in types() {
            for rows in [0usize, 1, 7, 8, 129, 8192] {
                for nulls in [false, true] {
                    let vector = random_vector(&mut rng, data_type, rows, nulls);
                    let bytes = block_bytes(&vector).unwrap();
                    let header = BlockHeader::parse(&bytes).unwrap();
                    assert_eq!(header.block_len(), bytes.len());
                    assert_eq!(header.rows as usize, rows);
                    assert_eq!(
                        header.flags & FLAG_VALIDITY != 0,
                        vector.null_count() > 0,
                        "{data_type}"
                    );
                    let (read, len) = read_block(&bytes, data_type, true, rows, "p").unwrap();
                    assert_eq!(len, bytes.len());
                    assert_same(&read, &vector);
                    if vector.null_count() == 0 {
                        assert!(read.validity.is_none());
                        let exact = read_block_exact(&bytes, data_type, false, rows, "p");
                        assert_same(&exact.unwrap(), &vector);
                    }
                }
            }
        }
    }

    #[test]
    fn repetitive_payload_is_compressed() {
        let values: Vec<Value> = (0..8192)
            .map(|i| Value::Float64(if i % 2 == 0 { 1.5 } else { 2.5 }))
            .collect();
        let vector = Vector::from_values(DataType::Float64, &values).unwrap();
        let bytes = block_bytes(&vector).unwrap();
        let header = BlockHeader::parse(&bytes).unwrap();
        assert_eq!(header.compressor, COMPRESSOR_VLZ);
        assert!(bytes.len() < 8192 * 8 / 2);
        let (read, _) = read_block(&bytes, DataType::Float64, false, 8192, "p").unwrap();
        assert_same(&read, &vector);
    }

    #[test]
    fn incompressible_payload_is_stored_raw() {
        let mut rng = Rng::new(3);
        let values: Vec<Value> = (0..1000)
            .map(|_| Value::Float64(f64::from_bits(rng.next_u64() >> 2)))
            .collect();
        let vector = Vector::from_values(DataType::Float64, &values).unwrap();
        let bytes = block_bytes(&vector).unwrap();
        assert_eq!(
            BlockHeader::parse(&bytes).unwrap().compressor,
            COMPRESSOR_NONE
        );
    }

    #[test]
    fn every_changed_byte_is_corrupt() {
        let mut rng = Rng::new(5);
        for (data_type, nulls) in [(DataType::Varchar, true), (DataType::Int64, false)] {
            let vector = random_vector(&mut rng, data_type, 300, nulls);
            let bytes = block_bytes(&vector).unwrap();
            for at in 0..bytes.len() {
                let mut bad = bytes.clone();
                bad[at] ^= 0x01;
                let err = read_block_exact(&bad, data_type, true, 300, "all_1_1_0").unwrap_err();
                assert!(
                    matches!(&err, VetaError::Corrupt { part, .. } if part == "all_1_1_0"),
                    "byte {at}: {err:?}"
                );
            }
        }
    }

    #[test]
    fn structural_failures_are_corrupt() {
        let vector = Vector::from_values(DataType::Int32, &[Value::Int32(1), Value::Null]).unwrap();
        let bytes = block_bytes(&vector).unwrap();
        let is_corrupt = |r: Result<Vector>| matches!(r, Err(VetaError::Corrupt { .. }));
        assert!(is_corrupt(read_block_exact(
            &bytes,
            DataType::Int32,
            true,
            3,
            "p"
        )));
        assert!(is_corrupt(read_block_exact(
            &bytes,
            DataType::Int32,
            false,
            2,
            "p"
        )));
        assert!(is_corrupt(read_block_exact(
            &bytes[..bytes.len() - 1],
            DataType::Int32,
            true,
            2,
            "p"
        )));
        let mut longer = bytes.clone();
        longer.push(0);
        assert!(is_corrupt(read_block_exact(
            &longer,
            DataType::Int32,
            true,
            2,
            "p"
        )));
        assert!(is_corrupt(read_block_exact(
            &bytes[..5],
            DataType::Int32,
            true,
            2,
            "p"
        )));
    }
}
