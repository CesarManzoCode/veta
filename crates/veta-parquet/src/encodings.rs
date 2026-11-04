//! Decoders of the Parquet value and level encodings (`Encodings.md`, parquet-format
//! 2.14.0, contract §9): PLAIN for every physical type, the RLE/bit-packing hybrid,
//! DELTA_BINARY_PACKED, DELTA_LENGTH_BYTE_ARRAY, DELTA_BYTE_ARRAY and BYTE_STREAM_SPLIT.
//! Dictionary indices are the hybrid with a leading bit-width byte ([`decode_dict_indices`]).
//!
//! Every decoder appends to caller-owned typed buffers, so a page decodes straight into
//! the buffers that become a `Vector`. Malformed or truncated input is `Err`, never a
//! panic. Arithmetic of DELTA_BINARY_PACKED wraps in two's complement, as the spec asks.

use crate::invalid;
use veta_core::Result;

/// Reads an unsigned LEB128 varint at `*pos` and advances it.
pub fn read_uleb128(data: &[u8], pos: &mut usize) -> Result<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let Some(&byte) = data.get(*pos) else {
            return Err(invalid("truncated varint"));
        };
        *pos += 1;
        if shift == 63 && byte > 1 {
            return Err(invalid("varint longer than 64 bits"));
        }
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
        if shift > 63 {
            return Err(invalid("varint longer than 64 bits"));
        }
    }
}

/// Reads a zigzag LEB128 varint at `*pos` and advances it.
pub fn read_zigzag(data: &[u8], pos: &mut usize) -> Result<i64> {
    let v = read_uleb128(data, pos)?;
    Ok(((v >> 1) as i64) ^ -((v & 1) as i64))
}

fn take<'a>(data: &'a [u8], pos: &mut usize, len: usize, what: &str) -> Result<&'a [u8]> {
    let end = pos
        .checked_add(len)
        .filter(|&end| end <= data.len())
        .ok_or_else(|| {
            invalid(format!(
                "{what} truncated: need {len} bytes at offset {}, have {}",
                *pos,
                data.len().saturating_sub(*pos)
            ))
        })?;
    let out = &data[*pos..end];
    *pos = end;
    Ok(out)
}

fn read_u32_le(data: &[u8], pos: &mut usize, what: &str) -> Result<u32> {
    let b = take(data, pos, 4, what)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Value `i` of a stream packed LSB-first at `width` bits (0–64). The caller has checked
/// that `bytes` holds `ceil((i + 1) · width / 8)` bytes.
#[inline]
fn bits_at(bytes: &[u8], i: usize, width: u32) -> u64 {
    if width == 0 {
        return 0;
    }
    let bit = i * width as usize;
    let (start, shift) = (bit / 8, (bit % 8) as u32);
    let window = if start + 16 <= bytes.len() {
        u128::from_le_bytes(bytes[start..start + 16].try_into().expect("16 bytes"))
    } else {
        let mut buf = [0u8; 16];
        let end = bytes.len().min(start + 16);
        buf[..end - start].copy_from_slice(&bytes[start..end]);
        u128::from_le_bytes(buf)
    };
    let mask = if width == 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    };
    ((window >> shift) as u64) & mask
}

/// Appends `count` values packed LSB-first at `width` bits from the start of `bytes`
/// (the order of the RLE/bit-packing hybrid and of DELTA_BINARY_PACKED mini-blocks).
pub fn unpack_lsb(bytes: &[u8], count: usize, width: u8, out: &mut Vec<u64>) -> Result<()> {
    if width > 64 {
        return Err(invalid(format!("bit width {width} is larger than 64")));
    }
    let needed = (count as u128 * u128::from(width)).div_ceil(8);
    if needed > bytes.len() as u128 {
        return Err(invalid(format!(
            "bit-packed values truncated: need {needed} bytes, have {}",
            bytes.len()
        )));
    }
    out.reserve(count);
    let w = u32::from(width);
    out.extend((0..count).map(|i| bits_at(bytes, i, w)));
    Ok(())
}

/// Decodes `count` values of the RLE/bit-packing hybrid (without the 4-byte length
/// prefix) at `bit_width` bits (0–32) from `data`, calling `emit(value, run_len)` for each
/// run of equal values (bit-packed values come one by one). Returns the bytes consumed.
/// A bit-packed run may be cut by the end of `data` if the values it still needs fit.
fn decode_hybrid_runs(
    data: &[u8],
    bit_width: u8,
    count: usize,
    mut emit: impl FnMut(u32, usize),
) -> Result<usize> {
    if bit_width > 32 {
        return Err(invalid(format!(
            "RLE bit width {bit_width} is larger than 32"
        )));
    }
    let value_bytes = usize::from(bit_width).div_ceil(8);
    let w = u32::from(bit_width);
    let mut pos = 0;
    let mut done = 0;
    while done < count {
        let header = read_uleb128(data, &mut pos)?;
        if header & 1 == 1 {
            let groups =
                usize::try_from(header >> 1).map_err(|_| invalid("bit-packed run too long"))?;
            let values = groups
                .checked_mul(8)
                .ok_or_else(|| invalid("bit-packed run too long"))?;
            if values == 0 {
                return Err(invalid("empty bit-packed run"));
            }
            let run_bytes = groups
                .checked_mul(usize::from(bit_width))
                .ok_or_else(|| invalid("bit-packed run too long"))?;
            let wanted = values.min(count - done);
            let available = data.len() - pos;
            let needed = (wanted * usize::from(bit_width)).div_ceil(8);
            if needed > available {
                return Err(invalid(format!(
                    "bit-packed run truncated: need {needed} bytes, have {available}"
                )));
            }
            let packed = &data[pos..pos + run_bytes.min(available)];
            for i in 0..wanted {
                emit(bits_at(packed, i, w) as u32, 1);
            }
            pos += run_bytes.min(available);
            done += wanted;
        } else {
            let run = usize::try_from(header >> 1).map_err(|_| invalid("RLE run too long"))?;
            if run == 0 {
                return Err(invalid("empty RLE run"));
            }
            let raw = take(data, &mut pos, value_bytes, "RLE run value")?;
            let mut value = 0u32;
            for (k, &b) in raw.iter().enumerate() {
                value |= u32::from(b) << (8 * k);
            }
            if w < 32 && value >> w != 0 {
                return Err(invalid(format!(
                    "RLE value {value} does not fit {bit_width} bits"
                )));
            }
            let n = run.min(count - done);
            emit(value, n);
            done += n;
        }
    }
    Ok(pos)
}

/// Appends `count` values of the RLE/bit-packing hybrid (no length prefix) to `out`;
/// returns the bytes consumed.
pub fn decode_rle_hybrid(
    data: &[u8],
    bit_width: u8,
    count: usize,
    out: &mut Vec<u32>,
) -> Result<usize> {
    out.reserve(count);
    decode_hybrid_runs(data, bit_width, count, |v, n| {
        if n == 1 {
            out.push(v);
        } else {
            out.extend(std::iter::repeat_n(v, n));
        }
    })
}

/// Decodes `count` definition levels of a flat column (bit width 1) into `valid`
/// (`true` = level 1 = value present) and returns the bytes consumed. With
/// `length_prefixed` the data starts with the 4-byte length of page v1.
pub fn decode_def_levels(
    data: &[u8],
    count: usize,
    length_prefixed: bool,
    valid: &mut Vec<bool>,
) -> Result<usize> {
    let (body, prefix) = if length_prefixed {
        let mut pos = 0;
        let len = read_u32_le(data, &mut pos, "definition levels length")? as usize;
        (take(data, &mut pos, len, "definition levels")?, 4)
    } else {
        (data, 0)
    };
    valid.reserve(count);
    let used = decode_hybrid_runs(body, 1, count, |v, n| {
        valid.extend(std::iter::repeat_n(v == 1, n));
    })?;
    Ok(if length_prefixed {
        prefix + body.len()
    } else {
        used
    })
}

/// Booleans encoded with RLE in a data page: a 4-byte length, then the hybrid at width 1
/// (both page versions, `Encodings.md`). Returns the bytes consumed.
pub fn decode_rle_booleans(data: &[u8], count: usize, out: &mut Vec<bool>) -> Result<usize> {
    let mut pos = 0;
    let len = read_u32_le(data, &mut pos, "RLE boolean length")? as usize;
    let body = take(data, &mut pos, len, "RLE booleans")?;
    out.reserve(count);
    decode_hybrid_runs(body, 1, count, |v, n| {
        out.extend(std::iter::repeat_n(v == 1, n));
    })?;
    Ok(pos)
}

/// Dictionary indices of a data page: a bit-width byte (≤ 32), then the hybrid without a
/// length prefix.
pub fn decode_dict_indices(data: &[u8], count: usize, out: &mut Vec<u32>) -> Result<usize> {
    let Some(&bit_width) = data.first() else {
        if count == 0 {
            return Ok(0);
        }
        return Err(invalid("dictionary indices without a bit width"));
    };
    Ok(1 + decode_rle_hybrid(&data[1..], bit_width, count, out)?)
}

// ---- PLAIN ----

fn plain_fixed<'a>(data: &'a [u8], count: usize, width: usize, what: &str) -> Result<&'a [u8]> {
    let len = count
        .checked_mul(width)
        .ok_or_else(|| invalid(format!("{what}: too many values")))?;
    let mut pos = 0;
    take(data, &mut pos, len, what)
}

/// PLAIN BOOLEAN: `count` bits, LSB first. Returns the bytes consumed.
pub fn plain_bool(data: &[u8], count: usize, out: &mut Vec<bool>) -> Result<usize> {
    let bytes = count.div_ceil(8);
    let mut pos = 0;
    let raw = take(data, &mut pos, bytes, "PLAIN booleans")?;
    out.reserve(count);
    out.extend((0..count).map(|i| raw[i / 8] >> (i % 8) & 1 == 1));
    Ok(bytes)
}

/// PLAIN INT32: 4 bytes little-endian each.
pub fn plain_i32(data: &[u8], count: usize, out: &mut Vec<i32>) -> Result<usize> {
    let raw = plain_fixed(data, count, 4, "PLAIN INT32")?;
    out.extend(
        raw.chunks_exact(4)
            .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]])),
    );
    Ok(raw.len())
}

/// PLAIN INT64: 8 bytes little-endian each.
pub fn plain_i64(data: &[u8], count: usize, out: &mut Vec<i64>) -> Result<usize> {
    let raw = plain_fixed(data, count, 8, "PLAIN INT64")?;
    out.extend(
        raw.chunks_exact(8)
            .map(|c| i64::from_le_bytes(c.try_into().expect("8 bytes"))),
    );
    Ok(raw.len())
}

/// PLAIN FLOAT, widened to `f64`.
pub fn plain_f32(data: &[u8], count: usize, out: &mut Vec<f64>) -> Result<usize> {
    let raw = plain_fixed(data, count, 4, "PLAIN FLOAT")?;
    out.extend(
        raw.chunks_exact(4)
            .map(|c| f64::from(f32::from_le_bytes([c[0], c[1], c[2], c[3]]))),
    );
    Ok(raw.len())
}

/// PLAIN DOUBLE.
pub fn plain_f64(data: &[u8], count: usize, out: &mut Vec<f64>) -> Result<usize> {
    let raw = plain_fixed(data, count, 8, "PLAIN DOUBLE")?;
    out.extend(
        raw.chunks_exact(8)
            .map(|c| f64::from_le_bytes(c.try_into().expect("8 bytes"))),
    );
    Ok(raw.len())
}

/// Variable-length byte strings appended as in a `Varchar` vector: string `i` is
/// `bytes[offsets[i]..offsets[i + 1]]`, and `offsets` already holds its leading 0.
pub struct ByteArrays<'a> {
    pub offsets: &'a mut Vec<u32>,
    pub bytes: &'a mut Vec<u8>,
}

impl ByteArrays<'_> {
    fn push(&mut self, value: &[u8]) -> Result<()> {
        self.bytes.extend_from_slice(value);
        self.close()
    }

    fn close(&mut self) -> Result<()> {
        let end = u32::try_from(self.bytes.len())
            .map_err(|_| invalid("byte arrays of a column chunk exceed 4 GiB"))?;
        self.offsets.push(end);
        Ok(())
    }

    fn last_start(&self) -> usize {
        self.offsets[self.offsets.len() - 1] as usize
    }
}

/// PLAIN BYTE_ARRAY: each value is a 4-byte little-endian length and the bytes.
pub fn plain_byte_array(data: &[u8], count: usize, out: &mut ByteArrays<'_>) -> Result<usize> {
    let mut pos = 0;
    out.offsets.reserve(count);
    for _ in 0..count {
        let len = read_u32_le(data, &mut pos, "PLAIN BYTE_ARRAY length")? as usize;
        let value = take(data, &mut pos, len, "PLAIN BYTE_ARRAY")?;
        out.push(value)?;
    }
    Ok(pos)
}

/// PLAIN FIXED_LEN_BYTE_ARRAY (and INT96 as width 12): `count · width` bytes.
pub fn plain_fixed_len(
    data: &[u8],
    count: usize,
    width: usize,
    out: &mut ByteArrays<'_>,
) -> Result<usize> {
    let raw = plain_fixed(data, count, width, "PLAIN FIXED_LEN_BYTE_ARRAY")?;
    out.offsets.reserve(count);
    if width == 0 {
        for _ in 0..count {
            out.close()?;
        }
    } else {
        for value in raw.chunks_exact(width) {
            out.push(value)?;
        }
    }
    Ok(raw.len())
}

// ---- DELTA_BINARY_PACKED ----

/// Decodes a whole DELTA_BINARY_PACKED stream, appending its values to `out`, and returns
/// `(value count, bytes consumed)`. Mini-blocks of the last block that hold no values have
/// no body; a needed mini-block is padded to its full size, but the padding may be cut by
/// the end of `data`. INT32 columns truncate the result (`as i32`), which is exact
/// because the wrapping arithmetic is the same modulo 2³².
pub fn decode_delta_binary_packed(data: &[u8], out: &mut Vec<i64>) -> Result<(usize, usize)> {
    let mut pos = 0;
    let block_size = read_uleb128(data, &mut pos)?;
    let minis = read_uleb128(data, &mut pos)?;
    let total = read_uleb128(data, &mut pos)?;
    let first = read_zigzag(data, &mut pos)?;
    if block_size == 0 || minis == 0 || block_size % minis != 0 {
        return Err(invalid(format!(
            "DELTA_BINARY_PACKED block of {block_size} values with {minis} mini-blocks"
        )));
    }
    let per_mini = block_size / minis;
    if per_mini % 8 != 0 || block_size > 1 << 20 {
        return Err(invalid(format!(
            "DELTA_BINARY_PACKED mini-block of {per_mini} values"
        )));
    }
    let (minis, per_mini) = (minis as usize, per_mini as usize);
    let total = usize::try_from(total).map_err(|_| invalid("DELTA_BINARY_PACKED too long"))?;
    // Each value after the first takes at least one bit-width byte per block, so a count
    // larger than that bound is a lie; refuse it before reserving memory.
    if total > 1 && (total - 1) / (per_mini * minis) > data.len() {
        return Err(invalid(format!(
            "DELTA_BINARY_PACKED declares {total} values in {} bytes",
            data.len()
        )));
    }
    if total == 0 {
        return Ok((0, pos));
    }
    out.reserve(total);
    out.push(first);
    let mut last = first;
    let mut remaining = total - 1;
    let mut deltas = Vec::with_capacity(per_mini);
    while remaining > 0 {
        let min_delta = read_zigzag(data, &mut pos)?;
        let widths = take(data, &mut pos, minis, "DELTA_BINARY_PACKED bit widths")?;
        for &width in widths {
            if remaining == 0 {
                break;
            }
            if width > 64 {
                return Err(invalid(format!(
                    "DELTA_BINARY_PACKED bit width {width} is larger than 64"
                )));
            }
            let n = per_mini.min(remaining);
            deltas.clear();
            unpack_lsb(&data[pos..], n, width, &mut deltas)?;
            let full = per_mini * usize::from(width) / 8;
            pos += full.min(data.len() - pos);
            for &d in &deltas {
                last = last.wrapping_add(min_delta).wrapping_add(d as i64);
                out.push(last);
            }
            remaining -= n;
        }
    }
    Ok((total, pos))
}

/// Decodes DELTA_BINARY_PACKED lengths, rejecting negative ones.
fn delta_lengths(data: &[u8], what: &str) -> Result<(Vec<usize>, usize)> {
    let mut raw = Vec::new();
    let (_, used) = decode_delta_binary_packed(data, &mut raw)?;
    let lengths = raw
        .into_iter()
        .map(|v| usize::try_from(v).map_err(|_| invalid(format!("negative {what} {v}"))))
        .collect::<Result<Vec<_>>>()?;
    Ok((lengths, used))
}

/// DELTA_LENGTH_BYTE_ARRAY: DELTA_BINARY_PACKED lengths, then the bytes back to back.
/// Appends the strings to `out`; returns `(value count, bytes consumed)`.
pub fn decode_delta_length_byte_array(
    data: &[u8],
    out: &mut ByteArrays<'_>,
) -> Result<(usize, usize)> {
    let (lengths, mut pos) = delta_lengths(data, "byte array length")?;
    out.offsets.reserve(lengths.len());
    for &len in &lengths {
        let value = take(data, &mut pos, len, "DELTA_LENGTH_BYTE_ARRAY data")?;
        out.push(value)?;
    }
    Ok((lengths.len(), pos))
}

/// DELTA_BYTE_ARRAY: DELTA_BINARY_PACKED prefix lengths, then the suffixes as
/// DELTA_LENGTH_BYTE_ARRAY; value `i` is the first `prefix[i]` bytes of value `i − 1`
/// followed by suffix `i`. The first value of each call has no predecessor (prefix 0).
pub fn decode_delta_byte_array(data: &[u8], out: &mut ByteArrays<'_>) -> Result<(usize, usize)> {
    let (prefixes, mut pos) = delta_lengths(data, "prefix length")?;
    let (suffix_lengths, used) = delta_lengths(&data[pos..], "suffix length")?;
    pos += used;
    if suffix_lengths.len() != prefixes.len() {
        return Err(invalid(format!(
            "DELTA_BYTE_ARRAY has {} prefixes and {} suffixes",
            prefixes.len(),
            suffix_lengths.len()
        )));
    }
    out.offsets.reserve(prefixes.len());
    let mut prev: Option<(usize, usize)> = None;
    for (&prefix, &suffix_len) in prefixes.iter().zip(&suffix_lengths) {
        let start = out.last_start();
        if prefix > 0 {
            match prev {
                Some((p_start, p_len)) if prefix <= p_len => {
                    out.bytes.extend_from_within(p_start..p_start + prefix);
                }
                _ => {
                    return Err(invalid(format!(
                        "DELTA_BYTE_ARRAY prefix {prefix} is longer than the previous value"
                    )));
                }
            }
        }
        let suffix = take(data, &mut pos, suffix_len, "DELTA_BYTE_ARRAY suffix")?;
        out.push(suffix)?;
        prev = Some((start, prefix + suffix_len));
    }
    Ok((prefixes.len(), pos))
}

// ---- BYTE_STREAM_SPLIT ----

/// BYTE_STREAM_SPLIT of `count` values of `K` bytes: byte `j` of value `i` is at
/// `data[j · count + i]`. The streams fill the data exactly. Calls `emit` with each value's
/// bytes in order.
pub fn byte_stream_split<const K: usize>(
    data: &[u8],
    count: usize,
    mut emit: impl FnMut([u8; K]),
) -> Result<()> {
    byte_stream_split_check(data, count, K)?;
    for i in 0..count {
        let mut value = [0u8; K];
        for (j, b) in value.iter_mut().enumerate() {
            *b = data[j * count + i];
        }
        emit(value);
    }
    Ok(())
}

fn byte_stream_split_check(data: &[u8], count: usize, width: usize) -> Result<()> {
    if count.checked_mul(width) != Some(data.len()) {
        return Err(invalid(format!(
            "BYTE_STREAM_SPLIT of {count} values of {width} bytes in {} bytes",
            data.len()
        )));
    }
    Ok(())
}

/// BYTE_STREAM_SPLIT of FIXED_LEN_BYTE_ARRAY values of `width` bytes.
pub fn byte_stream_split_fixed(
    data: &[u8],
    count: usize,
    width: usize,
    out: &mut ByteArrays<'_>,
) -> Result<()> {
    byte_stream_split_check(data, count, width)?;
    out.offsets.reserve(count);
    for i in 0..count {
        out.bytes.extend((0..width).map(|j| data[j * count + i]));
        out.close()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(offsets: &[u32], bytes: &[u8]) -> Vec<Vec<u8>> {
        offsets
            .windows(2)
            .map(|w| bytes[w[0] as usize..w[1] as usize].to_vec())
            .collect()
    }

    /// Unsigned LEB128 of `v`.
    fn uleb(v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        let mut v = v;
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

    /// Packs values LSB-first at `width` bits into exactly `ceil(n·width/8)` bytes.
    fn pack(values: &[u64], width: u32) -> Vec<u8> {
        let mut out = vec![0u8; (values.len() * width as usize).div_ceil(8)];
        for (i, &v) in values.iter().enumerate() {
            for b in 0..width as usize {
                if v >> b & 1 == 1 {
                    let bit = i * width as usize + b;
                    out[bit / 8] |= 1 << (bit % 8);
                }
            }
        }
        out
    }

    #[test]
    fn varints() {
        let data = [0x96, 0x01, 0x03, 0x01];
        let mut pos = 0;
        assert_eq!(read_uleb128(&data, &mut pos).unwrap(), 150);
        assert_eq!(read_zigzag(&data, &mut pos).unwrap(), -2);
        assert_eq!(read_zigzag(&data, &mut pos).unwrap(), -1);
        assert_eq!(pos, 4);
        assert!(read_uleb128(&[0x80], &mut 0).is_err());
        assert!(read_uleb128(&[0xFF; 11], &mut 0).is_err());
    }

    #[test]
    fn bit_packing_example_of_the_spec() {
        // Encodings.md: 0..7 at width 3 are 10001000 11000110 11111010.
        let bytes = [0b1000_1000, 0b1100_0110, 0b1111_1010];
        let mut out = Vec::new();
        unpack_lsb(&bytes, 8, 3, &mut out).unwrap();
        assert_eq!(out, (0..8).collect::<Vec<u64>>());
        assert!(unpack_lsb(&bytes, 9, 3, &mut out).is_err());
        assert!(unpack_lsb(&bytes, 1, 65, &mut out).is_err());
    }

    #[test]
    fn unpack_wide_values() {
        let values = [u64::MAX, 1, 1 << 63, 0x0123_4567_89AB_CDEF];
        let bytes = pack(&values, 64);
        let mut out = Vec::new();
        unpack_lsb(&bytes, 4, 64, &mut out).unwrap();
        assert_eq!(out, values);
        let values: Vec<u64> = (0..20)
            .map(|i| (i * 0x1_0000_0001) & ((1 << 33) - 1))
            .collect();
        let mut out = Vec::new();
        unpack_lsb(&pack(&values, 33), 20, 33, &mut out).unwrap();
        assert_eq!(out, values);
    }

    #[test]
    fn rle_hybrid_mixes_runs() {
        // RLE run of 5 × 4 at width 3, then a bit-packed run of 8 values 0..7 (the spec
        // example), then RLE run of 300 × 7.
        let mut data = vec![5 << 1, 4];
        data.push((1 << 1) | 1);
        data.extend_from_slice(&[0b1000_1000, 0b1100_0110, 0b1111_1010]);
        data.extend(uleb(300 << 1));
        data.push(7);
        let mut out = Vec::new();
        let used = decode_rle_hybrid(&data, 3, 313, &mut out).unwrap();
        assert_eq!(used, data.len());
        let mut expected = vec![4; 5];
        expected.extend(0..8);
        expected.extend(vec![7; 300]);
        assert_eq!(out, expected);
    }

    #[test]
    fn rle_hybrid_stops_inside_a_run() {
        // A bit-packed run of 8 slots holding only 3 real values, and an RLE run longer
        // than needed: only `count` values come out.
        let data = [(1 << 1) | 1, 0b0000_0101];
        let mut out = Vec::new();
        decode_rle_hybrid(&data, 1, 3, &mut out).unwrap();
        assert_eq!(out, [1, 0, 1]);
        let mut out = Vec::new();
        let mut data = uleb(100 << 1);
        data.extend_from_slice(&[9, 0]);
        decode_rle_hybrid(&data, 9, 4, &mut out).unwrap();
        assert_eq!(out, [9; 4]);
    }

    #[test]
    fn rle_hybrid_errors() {
        let mut out = Vec::new();
        assert!(decode_rle_hybrid(&[], 1, 1, &mut out).is_err());
        assert!(decode_rle_hybrid(&[0], 1, 1, &mut out).is_err()); // empty RLE run
        assert!(decode_rle_hybrid(&[2 << 1, 5], 2, 2, &mut out).is_err()); // 5 > 2 bits
        assert!(decode_rle_hybrid(&[(2 << 1) | 1, 0xFF], 8, 16, &mut out).is_err());
        assert!(decode_rle_hybrid(&[2, 0], 33, 1, &mut out).is_err());
    }

    #[test]
    fn rle_hybrid_width_zero() {
        let mut out = Vec::new();
        decode_rle_hybrid(&[10 << 1], 0, 10, &mut out).unwrap();
        assert_eq!(out, [0; 10]);
    }

    #[test]
    fn def_levels_and_booleans() {
        let mut body = vec![(1 << 1) | 1, 0b0001_1011];
        let mut data = (body.len() as u32).to_le_bytes().to_vec();
        data.append(&mut body);
        let mut valid = Vec::new();
        assert_eq!(decode_def_levels(&data, 5, true, &mut valid).unwrap(), 6);
        assert_eq!(valid, [true, true, false, true, true]);
        let mut valid = Vec::new();
        assert_eq!(
            decode_def_levels(&data[4..], 5, false, &mut valid).unwrap(),
            2
        );
        assert_eq!(valid, [true, true, false, true, true]);
        let mut bools = Vec::new();
        decode_rle_booleans(&[2, 0, 0, 0, 3 << 1, 1], 3, &mut bools).unwrap();
        assert_eq!(bools, [true; 3]);
        assert!(decode_def_levels(&[9, 0, 0, 0, 0], 1, true, &mut valid).is_err());
    }

    #[test]
    fn dict_indices() {
        let mut out = Vec::new();
        let used =
            decode_dict_indices(&[2, (1 << 1) | 1, 0b1110_0100, 0b0000_0000], 4, &mut out).unwrap();
        assert_eq!(out, [0, 1, 2, 3]);
        assert_eq!(used, 4);
        assert!(decode_dict_indices(&[], 1, &mut out).is_err());
    }

    #[test]
    fn plain_every_physical_type() {
        let mut bools = Vec::new();
        assert_eq!(plain_bool(&[0b0000_0101, 0b1], 9, &mut bools).unwrap(), 2);
        assert_eq!(
            bools,
            [true, false, true, false, false, false, false, false, true]
        );

        let mut ints = Vec::new();
        let data: Vec<u8> = [1i32, -2, i32::MAX]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        assert_eq!(plain_i32(&data, 3, &mut ints).unwrap(), 12);
        assert_eq!(ints, [1, -2, i32::MAX]);

        let mut longs = Vec::new();
        let data: Vec<u8> = [i64::MIN, 7].iter().flat_map(|v| v.to_le_bytes()).collect();
        plain_i64(&data, 2, &mut longs).unwrap();
        assert_eq!(longs, [i64::MIN, 7]);

        let mut floats = Vec::new();
        let data: Vec<u8> = [1.5f32, -0.25]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        plain_f32(&data, 2, &mut floats).unwrap();
        assert_eq!(floats, [1.5, -0.25]);

        let mut doubles = Vec::new();
        let data: Vec<u8> = [0.1f64, 1e300]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        plain_f64(&data, 2, &mut doubles).unwrap();
        assert_eq!(doubles, [0.1, 1e300]);

        let (mut offsets, mut bytes) = (vec![0u32], Vec::new());
        let mut data = 2u32.to_le_bytes().to_vec();
        data.extend_from_slice(b"ab");
        data.extend_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&3u32.to_le_bytes());
        data.extend_from_slice(b"xyz");
        let mut out = ByteArrays {
            offsets: &mut offsets,
            bytes: &mut bytes,
        };
        assert_eq!(plain_byte_array(&data, 3, &mut out).unwrap(), data.len());
        assert!(plain_byte_array(&data[..5], 1, &mut out).is_err());
        assert_eq!(
            strings(&offsets[..4], &bytes),
            [b"ab".to_vec(), b"".to_vec(), b"xyz".to_vec()]
        );

        let (mut offsets, mut bytes) = (vec![0u32], Vec::new());
        let mut out = ByteArrays {
            offsets: &mut offsets,
            bytes: &mut bytes,
        };
        plain_fixed_len(b"abcdefghijkl", 4, 3, &mut out).unwrap();
        assert_eq!(strings(&offsets, &bytes)[3], b"jkl");
        let mut ints = Vec::new();
        assert!(plain_i32(&[0; 7], 2, &mut ints).is_err());
    }

    /// The header of DELTA_BINARY_PACKED.
    fn delta_header(block: u64, minis: u64, total: u64, first: i64) -> Vec<u8> {
        let mut out = uleb(block);
        out.extend(uleb(minis));
        out.extend(uleb(total));
        out.extend(zz(first));
        out
    }

    #[test]
    fn delta_binary_packed_examples_of_the_spec() {
        // Example 1 (block size 8 is invalid in real files but the reader only needs a
        // multiple of 8 per mini-block): 1, 2, 3, 4, 5.
        let mut data = delta_header(8, 1, 5, 1);
        data.extend(zz(1));
        data.push(0);
        let mut out = Vec::new();
        assert_eq!(
            decode_delta_binary_packed(&data, &mut out).unwrap(),
            (5, data.len())
        );
        assert_eq!(out, [1, 2, 3, 4, 5]);

        // Example 2: 7, 5, 3, 1, 2, 3, 4, 5 with min delta −2 and width 2.
        let mut data = delta_header(8, 1, 8, 7);
        data.extend(zz(-2));
        data.push(2);
        data.extend(pack(&[0, 0, 0, 3, 3, 3, 3, 0], 2));
        let mut out = Vec::new();
        assert_eq!(
            decode_delta_binary_packed(&data, &mut out).unwrap(),
            (8, data.len())
        );
        assert_eq!(out, [7, 5, 3, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn delta_binary_packed_several_mini_blocks_and_blocks() {
        // Block of 128 values in 4 mini-blocks of 32, two blocks; mini-block widths 0, 5,
        // 0 and 7 in the first block; the second block needs only one of its mini-blocks
        // (the others have no body even though their widths are garbage).
        let n = 1 + 128 + 10;
        let mut values = vec![100i64];
        let mut deltas_block1 = Vec::new();
        for m in 0..4 {
            for i in 0..32u64 {
                let d = match m {
                    0 | 2 => 0,
                    1 => i % 31,
                    _ => (i * 3) % 127,
                };
                deltas_block1.push(d);
            }
        }
        for &d in &deltas_block1 {
            let last = *values.last().unwrap();
            values.push(last + d as i64 - 5);
        }
        let deltas_block2: Vec<u64> = (0..10).map(|i| i * 2).collect();
        for &d in &deltas_block2 {
            let last = *values.last().unwrap();
            values.push(last + d as i64 + 1000);
        }
        let mut data = delta_header(128, 4, n as u64, 100);
        data.extend(zz(-5));
        data.extend_from_slice(&[0, 5, 0, 7]);
        data.extend(pack(&deltas_block1[32..64], 5));
        data.extend(pack(&deltas_block1[96..128], 7));
        data.extend(zz(1000));
        data.extend_from_slice(&[5, 0xEE, 0xEE, 0xEE]);
        let mut second = deltas_block2.clone();
        second.resize(32, 0);
        data.extend(pack(&second, 5));
        data.extend_from_slice(b"tail");
        let mut out = Vec::new();
        let (count, used) = decode_delta_binary_packed(&data, &mut out).unwrap();
        assert_eq!(count, n);
        assert_eq!(used, data.len() - 4);
        assert_eq!(out, values);
    }

    #[test]
    fn delta_binary_packed_wraps_and_handles_small_counts() {
        // One value: header only.
        let data = delta_header(128, 4, 1, -9);
        let mut out = Vec::new();
        assert_eq!(
            decode_delta_binary_packed(&data, &mut out).unwrap(),
            (1, data.len())
        );
        assert_eq!(out, [-9]);
        // Zero values.
        let data = delta_header(128, 4, 0, 0);
        let mut out = Vec::new();
        assert_eq!(decode_delta_binary_packed(&data, &mut out).unwrap().0, 0);
        assert!(out.is_empty());
        // i64::MAX then i64::MIN: delta wraps.
        let mut data = delta_header(128, 1, 2, i64::MAX);
        data.extend(zz(i64::MIN.wrapping_sub(i64::MAX)));
        data.push(0);
        let mut out = Vec::new();
        decode_delta_binary_packed(&data, &mut out).unwrap();
        assert_eq!(out, [i64::MAX, i64::MIN]);
        // INT32 wrapping: i32::MAX, i32::MIN encoded with an i32 delta of 1.
        let mut data = delta_header(128, 1, 2, i64::from(i32::MAX));
        data.extend(zz(1));
        data.push(0);
        let mut out = Vec::new();
        decode_delta_binary_packed(&data, &mut out).unwrap();
        assert_eq!(out[1] as i32, i32::MIN);
    }

    #[test]
    fn delta_binary_packed_errors() {
        let mut out = Vec::new();
        assert!(decode_delta_binary_packed(&delta_header(0, 1, 2, 0), &mut out).is_err());
        assert!(decode_delta_binary_packed(&delta_header(128, 3, 2, 0), &mut out).is_err());
        assert!(decode_delta_binary_packed(&delta_header(128, 4, 2, 0), &mut out).is_err());
        let mut data = delta_header(128, 1, 3, 0);
        data.extend(zz(0));
        data.push(65);
        assert!(decode_delta_binary_packed(&data, &mut out).is_err());
        assert!(
            decode_delta_binary_packed(&delta_header(128, 1, u64::MAX >> 2, 0), &mut out).is_err()
        );
    }

    /// DELTA_BINARY_PACKED of `values` in one block of 128 with one mini-block.
    fn delta_encode(values: &[i64]) -> Vec<u8> {
        assert!(values.len() <= 129);
        let mut data = delta_header(
            128,
            1,
            values.len() as u64,
            values.first().copied().unwrap_or(0),
        );
        if values.len() > 1 {
            let deltas: Vec<i64> = values.windows(2).map(|w| w[1] - w[0]).collect();
            let min = *deltas.iter().min().unwrap();
            let rel: Vec<u64> = deltas.iter().map(|d| (d - min) as u64).collect();
            let width = 64 - rel.iter().max().unwrap().leading_zeros();
            data.extend(zz(min));
            data.push(width as u8);
            let mut padded = rel.clone();
            padded.resize(128, 0);
            data.extend(pack(&padded, width));
        }
        data
    }

    #[test]
    fn delta_length_byte_array_example_of_the_spec() {
        let mut data = delta_encode(&[5, 5, 6, 6]);
        data.extend_from_slice(b"HelloWorldFoobarABCDEF");
        let (mut offsets, mut bytes) = (vec![0u32], Vec::new());
        let mut out = ByteArrays {
            offsets: &mut offsets,
            bytes: &mut bytes,
        };
        assert_eq!(
            decode_delta_length_byte_array(&data, &mut out).unwrap(),
            (4, data.len())
        );
        assert_eq!(
            strings(&offsets, &bytes),
            [
                b"Hello".to_vec(),
                b"World".to_vec(),
                b"Foobar".to_vec(),
                b"ABCDEF".to_vec()
            ]
        );
        let mut short = delta_encode(&[5, 5]);
        short.extend_from_slice(b"Hello");
        let (mut offsets, mut bytes) = (vec![0u32], Vec::new());
        let mut out = ByteArrays {
            offsets: &mut offsets,
            bytes: &mut bytes,
        };
        assert!(decode_delta_length_byte_array(&short, &mut out).is_err());
    }

    #[test]
    fn delta_byte_array_example_of_the_spec() {
        // "axis", "axle", "babble", "babyhood": prefixes 0, 2, 0, 3; suffix lengths 4, 2,
        // 6, 5; suffixes "axislebabbleyhood".
        let mut data = delta_encode(&[0, 2, 0, 3]);
        data.extend(delta_encode(&[4, 2, 6, 5]));
        data.extend_from_slice(b"axislebabbleyhood");
        let (mut offsets, mut bytes) = (vec![0u32], b"".to_vec());
        let mut out = ByteArrays {
            offsets: &mut offsets,
            bytes: &mut bytes,
        };
        assert_eq!(
            decode_delta_byte_array(&data, &mut out).unwrap(),
            (4, data.len())
        );
        assert_eq!(
            strings(&offsets, &bytes),
            [
                b"axis".to_vec(),
                b"axle".to_vec(),
                b"babble".to_vec(),
                b"babyhood".to_vec()
            ]
        );
    }

    #[test]
    fn delta_byte_array_appends_after_existing_values() {
        // The buffers already hold "zz"; prefixes refer only to this call's values.
        let mut data = delta_encode(&[0, 1]);
        data.extend(delta_encode(&[2, 1]));
        data.extend_from_slice(b"abc");
        let (mut offsets, mut bytes) = (vec![0u32, 2], b"zz".to_vec());
        let mut out = ByteArrays {
            offsets: &mut offsets,
            bytes: &mut bytes,
        };
        decode_delta_byte_array(&data, &mut out).unwrap();
        assert_eq!(
            strings(&offsets, &bytes),
            [b"zz".to_vec(), b"ab".to_vec(), b"ac".to_vec()]
        );
    }

    #[test]
    fn delta_byte_array_bad_prefix_fails() {
        let mut data = delta_encode(&[1, 0]);
        data.extend(delta_encode(&[1, 1]));
        data.extend_from_slice(b"ab");
        let (mut offsets, mut bytes) = (vec![0u32], Vec::new());
        let mut out = ByteArrays {
            offsets: &mut offsets,
            bytes: &mut bytes,
        };
        assert!(decode_delta_byte_array(&data, &mut out).is_err());
    }

    #[test]
    fn byte_stream_split_example_of_the_spec() {
        let data = [
            0xAA, 0x00, 0xA3, 0xBB, 0x11, 0xB4, 0xCC, 0x22, 0xC5, 0xDD, 0x33, 0xD6,
        ];
        let mut values = Vec::new();
        byte_stream_split::<4>(&data, 3, |b| values.push(b)).unwrap();
        assert_eq!(
            values,
            [
                [0xAA, 0xBB, 0xCC, 0xDD],
                [0x00, 0x11, 0x22, 0x33],
                [0xA3, 0xB4, 0xC5, 0xD6]
            ]
        );
        assert!(byte_stream_split::<4>(&data, 2, |_| {}).is_err());
    }

    #[test]
    fn byte_stream_split_doubles() {
        let doubles = [1.0f64, -2.5, f64::MAX, 1e-300];
        let n = doubles.len();
        let mut data = vec![0u8; 8 * n];
        for (i, d) in doubles.iter().enumerate() {
            for (j, b) in d.to_le_bytes().iter().enumerate() {
                data[j * n + i] = *b;
            }
        }
        let mut out = Vec::new();
        byte_stream_split::<8>(&data, n, |b| out.push(f64::from_le_bytes(b))).unwrap();
        assert_eq!(out, doubles);

        let (mut offsets, mut bytes) = (vec![0u32], Vec::new());
        let mut fixed = ByteArrays {
            offsets: &mut offsets,
            bytes: &mut bytes,
        };
        byte_stream_split_fixed(&[1, 2, 3, 4, 5, 6], 2, 3, &mut fixed).unwrap();
        assert_eq!(strings(&offsets, &bytes), [vec![1, 3, 5], vec![2, 4, 6]]);
    }
}
