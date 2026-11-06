//! Encoders of the Parquet value and level encodings (`Encodings.md`, parquet-format 2.14.0,
//! contract §9): PLAIN, the RLE/bit-packing hybrid, DELTA_BINARY_PACKED (blocks of 128 values
//! in 4 mini-blocks), DELTA_LENGTH_BYTE_ARRAY and DELTA_BYTE_ARRAY. They are the inverse of
//! [`crate::encodings`].
//!
//! Every encoder appends to a caller-owned buffer. Differences of DELTA_BINARY_PACKED wrap in
//! two's complement at the width of the physical type, so an INT32 column never needs a
//! mini-block wider than 32 bits.

/// Values per block of DELTA_BINARY_PACKED.
const DELTA_BLOCK: usize = 128;
/// Mini-blocks per block of DELTA_BINARY_PACKED.
const DELTA_MINI_BLOCKS: usize = 4;
const DELTA_PER_MINI: usize = DELTA_BLOCK / DELTA_MINI_BLOCKS;

/// Shortest run of equal values worth an RLE run instead of a bit-packed group.
const RLE_MIN_RUN: usize = 8;

/// Appends `v` as an unsigned LEB128 varint.
pub fn write_uleb128(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Appends `v` as a zigzag LEB128 varint.
pub fn write_zigzag(out: &mut Vec<u8>, v: i64) {
    write_uleb128(out, ((v << 1) ^ (v >> 63)) as u64);
}

/// Appends `values` packed LSB-first at `width` bits (0–64) in `ceil(n · width / 8)` bytes.
/// Bits of a value above `width` are dropped.
pub fn pack_lsb(values: &[u64], width: u8, out: &mut Vec<u8>) {
    if width == 0 {
        return;
    }
    let mask = if width >= 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    };
    let mut bits = 0u128;
    let mut filled = 0u32;
    for &value in values {
        bits |= u128::from(value & mask) << filled;
        filled += u32::from(width);
        while filled >= 8 {
            out.push(bits as u8);
            bits >>= 8;
            filled -= 8;
        }
    }
    if filled > 0 {
        out.push(bits as u8);
    }
}

/// Smallest width that holds `max` (0 for `max == 0`).
fn width_for(max: u64) -> u8 {
    (64 - max.leading_zeros()) as u8
}

fn write_bit_packed_run(values: &[u32], bit_width: u8, out: &mut Vec<u8>) {
    if values.is_empty() {
        return;
    }
    let groups = values.len().div_ceil(8);
    write_uleb128(out, ((groups as u64) << 1) | 1);
    let wide: Vec<u64> = values.iter().map(|&v| u64::from(v)).collect();
    let start = out.len();
    pack_lsb(&wide, bit_width, out);
    // Only the last run of a stream can be short: a reader stops at its value count.
    out.resize(start + groups * usize::from(bit_width), 0);
}

fn write_rle_run(value: u32, run: usize, bit_width: u8, out: &mut Vec<u8>) {
    write_uleb128(out, (run as u64) << 1);
    let bytes = usize::from(bit_width).div_ceil(8);
    out.extend_from_slice(&value.to_le_bytes()[..bytes]);
}

/// Appends `values` in the RLE/bit-packing hybrid (no length prefix) at `bit_width` bits
/// (0–32). A run of equal values becomes an RLE run once it can complete the pending
/// bit-packed group and still leave at least [`RLE_MIN_RUN`] values.
pub fn encode_rle_hybrid(values: &[u32], bit_width: u8, out: &mut Vec<u8>) {
    let mut literal = 0;
    let mut i = 0;
    while i < values.len() {
        let value = values[i];
        let run = values[i..].iter().take_while(|&&v| v == value).count();
        let fill = (8 - (i - literal) % 8) % 8;
        if run >= fill + RLE_MIN_RUN {
            i += fill;
            write_bit_packed_run(&values[literal..i], bit_width, out);
            write_rle_run(value, run - fill, bit_width, out);
            i += run - fill;
            literal = i;
        } else {
            i += run;
        }
    }
    write_bit_packed_run(&values[literal..], bit_width, out);
}

/// Definition levels of a flat column (bit width 1, `true` = level 1) in the hybrid, without
/// the length prefix of page v1.
pub fn encode_def_levels(valid: &[bool], out: &mut Vec<u8>) {
    let levels: Vec<u32> = valid.iter().map(|&v| u32::from(v)).collect();
    encode_rle_hybrid(&levels, 1, out);
}

/// Bit width of the indices of a dictionary of `size` entries.
pub fn dict_bit_width(size: usize) -> u8 {
    width_for(size.saturating_sub(1) as u64)
}

/// Dictionary indices of a data page: a bit-width byte, then the hybrid.
pub fn encode_dict_indices(indices: &[u32], bit_width: u8, out: &mut Vec<u8>) {
    out.push(bit_width);
    encode_rle_hybrid(indices, bit_width, out);
}

// ---- PLAIN ----

/// PLAIN BOOLEAN: one bit per value, LSB first.
pub fn plain_bool(values: &[bool], out: &mut Vec<u8>) {
    for group in values.chunks(8) {
        let mut byte = 0u8;
        for (i, &value) in group.iter().enumerate() {
            byte |= u8::from(value) << i;
        }
        out.push(byte);
    }
}

/// PLAIN INT32: 4 bytes little-endian each.
pub fn plain_i32(values: &[i32], out: &mut Vec<u8>) {
    out.reserve(values.len() * 4);
    for v in values {
        out.extend_from_slice(&v.to_le_bytes());
    }
}

/// PLAIN INT64: 8 bytes little-endian each.
pub fn plain_i64(values: &[i64], out: &mut Vec<u8>) {
    out.reserve(values.len() * 8);
    for v in values {
        out.extend_from_slice(&v.to_le_bytes());
    }
}

/// PLAIN DOUBLE.
pub fn plain_f64(values: &[f64], out: &mut Vec<u8>) {
    out.reserve(values.len() * 8);
    for v in values {
        out.extend_from_slice(&v.to_le_bytes());
    }
}

/// PLAIN BYTE_ARRAY: each value is a 4-byte little-endian length and the bytes. The writer
/// has checked that the page fits an `i32`, so every length fits a `u32`.
pub fn plain_byte_array(values: &[&[u8]], out: &mut Vec<u8>) {
    for value in values {
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(value);
    }
}

// ---- DELTA_BINARY_PACKED ----

/// `values` in DELTA_BINARY_PACKED; with `narrow` the differences wrap at 32 bits.
fn encode_delta(values: &[i64], narrow: bool, out: &mut Vec<u8>) {
    write_uleb128(out, DELTA_BLOCK as u64);
    write_uleb128(out, DELTA_MINI_BLOCKS as u64);
    write_uleb128(out, values.len() as u64);
    write_zigzag(out, values.first().copied().unwrap_or(0));
    let deltas: Vec<i64> = values
        .windows(2)
        .map(|w| {
            if narrow {
                i64::from((w[1] as i32).wrapping_sub(w[0] as i32))
            } else {
                w[1].wrapping_sub(w[0])
            }
        })
        .collect();
    let mut adjusted = Vec::with_capacity(DELTA_BLOCK);
    for block in deltas.chunks(DELTA_BLOCK) {
        let min_delta = block.iter().copied().min().unwrap_or(0);
        write_zigzag(out, min_delta);
        adjusted.clear();
        adjusted.extend(block.iter().map(|&d| {
            if narrow {
                u64::from((d as i32).wrapping_sub(min_delta as i32) as u32)
            } else {
                d.wrapping_sub(min_delta) as u64
            }
        }));
        // A mini-block without values has width 0 and no body.
        let mut widths = [0u8; DELTA_MINI_BLOCKS];
        for (width, mini) in widths.iter_mut().zip(adjusted.chunks(DELTA_PER_MINI)) {
            *width = width_for(mini.iter().copied().max().unwrap_or(0));
        }
        out.extend_from_slice(&widths);
        for (&width, mini) in widths.iter().zip(adjusted.chunks(DELTA_PER_MINI)) {
            let start = out.len();
            pack_lsb(mini, width, out);
            // A mini-block with values is padded to its full size.
            out.resize(start + DELTA_PER_MINI * usize::from(width) / 8, 0);
        }
    }
}

/// DELTA_BINARY_PACKED of an INT64 column.
pub fn encode_delta_i64(values: &[i64], out: &mut Vec<u8>) {
    encode_delta(values, false, out);
}

/// DELTA_BINARY_PACKED of an INT32 column.
pub fn encode_delta_i32(values: &[i32], out: &mut Vec<u8>) {
    let wide: Vec<i64> = values.iter().map(|&v| i64::from(v)).collect();
    encode_delta(&wide, true, out);
}

fn encode_lengths(lengths: impl Iterator<Item = usize>, out: &mut Vec<u8>) {
    let lengths: Vec<i64> = lengths.map(|len| len as i64).collect();
    encode_delta(&lengths, true, out);
}

/// DELTA_LENGTH_BYTE_ARRAY: DELTA_BINARY_PACKED lengths, then the bytes back to back.
pub fn encode_delta_length_byte_array(values: &[&[u8]], out: &mut Vec<u8>) {
    encode_lengths(values.iter().map(|v| v.len()), out);
    for value in values {
        out.extend_from_slice(value);
    }
}

/// DELTA_BYTE_ARRAY: DELTA_BINARY_PACKED prefix lengths, then the suffixes as
/// DELTA_LENGTH_BYTE_ARRAY. The first value has no predecessor (prefix 0).
pub fn encode_delta_byte_array(values: &[&[u8]], out: &mut Vec<u8>) {
    let mut previous: &[u8] = &[];
    let mut prefixes = Vec::with_capacity(values.len());
    for value in values {
        let common = previous
            .iter()
            .zip(value.iter())
            .take_while(|(a, b)| a == b)
            .count();
        prefixes.push(common);
        previous = value;
    }
    encode_lengths(prefixes.iter().copied(), out);
    encode_lengths(values.iter().zip(&prefixes).map(|(v, &p)| v.len() - p), out);
    for (value, &prefix) in values.iter().zip(&prefixes) {
        out.extend_from_slice(&value[prefix..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encodings::{
        ByteArrays, decode_delta_binary_packed, decode_delta_byte_array,
        decode_delta_length_byte_array, decode_dict_indices, decode_rle_hybrid, read_uleb128,
        read_zigzag, unpack_lsb,
    };

    fn hybrid(values: &[u32], width: u8) -> Vec<u8> {
        let mut out = Vec::new();
        encode_rle_hybrid(values, width, &mut out);
        out
    }

    fn check_hybrid(values: &[u32], width: u8) {
        let bytes = hybrid(values, width);
        let mut back = Vec::new();
        let used = decode_rle_hybrid(&bytes, width, values.len(), &mut back).unwrap();
        assert_eq!(used, bytes.len(), "{values:?}");
        assert_eq!(back, values);
    }

    #[test]
    fn varints_read_back() {
        let mut out = Vec::new();
        write_uleb128(&mut out, 150);
        write_zigzag(&mut out, -2);
        write_zigzag(&mut out, i64::MIN);
        write_zigzag(&mut out, i64::MAX);
        let mut pos = 0;
        assert_eq!(read_uleb128(&out, &mut pos).unwrap(), 150);
        assert_eq!(read_zigzag(&out, &mut pos).unwrap(), -2);
        assert_eq!(read_zigzag(&out, &mut pos).unwrap(), i64::MIN);
        assert_eq!(read_zigzag(&out, &mut pos).unwrap(), i64::MAX);
        assert_eq!(pos, out.len());
    }

    #[test]
    fn bit_packing_example_of_the_spec() {
        // Encodings.md: 0..7 at width 3 are 10001000 11000110 11111010.
        let mut out = Vec::new();
        pack_lsb(&(0..8).collect::<Vec<u64>>(), 3, &mut out);
        assert_eq!(out, [0b1000_1000, 0b1100_0110, 0b1111_1010]);
    }

    #[test]
    fn packing_keeps_every_width() {
        for width in 0..=64u8 {
            let mask = if width == 64 {
                u64::MAX
            } else {
                (1u64 << width) - 1
            };
            let values: Vec<u64> = (0..50u64)
                .map(|i| i.wrapping_mul(0x9E37_79B9_7F4A_7C15) & mask)
                .collect();
            let mut out = Vec::new();
            pack_lsb(&values, width, &mut out);
            assert_eq!(out.len(), (50 * usize::from(width)).div_ceil(8));
            let mut back = Vec::new();
            unpack_lsb(&out, 50, width, &mut back).unwrap();
            assert_eq!(back, values, "width {width}");
        }
    }

    #[test]
    fn hybrid_uses_runs_and_groups() {
        // All equal: one RLE run.
        assert_eq!(hybrid(&[1; 50], 1), [50 << 1, 1]);
        // Short and different: one bit-packed group, padded to 8 values.
        assert_eq!(hybrid(&[1, 0, 1], 1), [(1 << 1) | 1, 0b101]);
        // The group in progress takes the first values of a long run.
        let mut values = vec![1, 0, 1];
        values.extend([1; 20]);
        assert_eq!(hybrid(&values, 1), [(1 << 1) | 1, 0b1111_1101, 15 << 1, 1]);
    }

    #[test]
    fn hybrid_round_trips() {
        check_hybrid(&[], 1);
        check_hybrid(&[0; 7], 0);
        check_hybrid(&[0; 700], 0);
        check_hybrid(&[1, 1, 1, 1, 1, 1, 1], 1);
        check_hybrid(&[5; 8], 3);
        check_hybrid(&[65_535; 300], 16);
        check_hybrid(&[1 << 16; 300], 17);
        check_hybrid(&[u32::MAX, 0, u32::MAX, 7], 32);
        // Patterns that end a group inside, before and after a run.
        for start in 0..9 {
            for run in [1, 7, 8, 9, 15, 16, 17, 40] {
                for tail in 0..9 {
                    let mut values: Vec<u32> = (0..start).map(|i| i % 4).collect();
                    values.extend(std::iter::repeat_n(6, run));
                    values.extend((0..tail).map(|i| i % 4));
                    check_hybrid(&values, 3);
                }
            }
        }
    }

    #[test]
    fn dictionary_indices_carry_their_width() {
        assert_eq!(dict_bit_width(1), 0);
        assert_eq!(dict_bit_width(2), 1);
        assert_eq!(dict_bit_width(3), 2);
        assert_eq!(dict_bit_width(256), 8);
        assert_eq!(dict_bit_width(257), 9);
        assert_eq!(dict_bit_width(65_536), 16);
        let indices: Vec<u32> = (0..1000).map(|i| i % 257).collect();
        let mut out = Vec::new();
        encode_dict_indices(&indices, dict_bit_width(257), &mut out);
        assert_eq!(out[0], 9);
        let mut back = Vec::new();
        assert_eq!(
            decode_dict_indices(&out, 1000, &mut back).unwrap(),
            out.len()
        );
        assert_eq!(back, indices);
    }

    #[test]
    fn plain_booleans_are_lsb_first() {
        let mut out = Vec::new();
        plain_bool(
            &[true, false, true, true, false, false, false, false, true],
            &mut out,
        );
        assert_eq!(out, [0b0000_1101, 1]);
    }

    fn check_delta_i64(values: &[i64]) {
        let mut out = Vec::new();
        encode_delta_i64(values, &mut out);
        let mut back = Vec::new();
        let (count, used) = decode_delta_binary_packed(&out, &mut back).unwrap();
        assert_eq!(count, values.len());
        assert_eq!(used, out.len());
        assert_eq!(back, values);
    }

    fn check_delta_i32(values: &[i32]) {
        let mut out = Vec::new();
        encode_delta_i32(values, &mut out);
        let mut wide = Vec::new();
        let (count, used) = decode_delta_binary_packed(&out, &mut wide).unwrap();
        assert_eq!(count, values.len());
        assert_eq!(used, out.len());
        let back: Vec<i32> = wide.iter().map(|&v| v as i32).collect();
        assert_eq!(back, values);
    }

    #[test]
    fn delta_header_follows_the_spec() {
        let mut out = Vec::new();
        encode_delta_i64(&[7, 5, 3, 1, 2, 3, 4, 5, 6], &mut out);
        // Block size, 4 mini-blocks, 9 values, first value 7, minimum delta −2.
        assert_eq!(out[..3], [0x80, 0x01, 4]);
        assert_eq!(out[3..6], [9, 14, 3]);
        // One mini-block with deltas 0..=3 over the minimum: width 2, 32 values, 8 bytes.
        assert_eq!(out[6..10], [2, 0, 0, 0]);
        assert_eq!(out.len(), 10 + 8);
    }

    #[test]
    fn delta_round_trips_lengths_around_block_borders() {
        for n in [
            0, 1, 2, 3, 8, 31, 32, 33, 64, 127, 128, 129, 130, 256, 257, 1000, 8192,
        ] {
            let values: Vec<i64> = (0..n as i64)
                .map(|i| i.wrapping_mul(i).wrapping_mul(-3) + i % 7)
                .collect();
            check_delta_i64(&values);
            let narrow: Vec<i32> = values.iter().map(|&v| v as i32).collect();
            check_delta_i32(&narrow);
        }
    }

    #[test]
    fn delta_wraps_at_the_extremes() {
        check_delta_i64(&[i64::MAX, i64::MIN, i64::MAX, 0, -1, i64::MIN]);
        check_delta_i32(&[i32::MAX, i32::MIN, i32::MAX, 0, -1, i32::MIN]);
        check_delta_i64(&[5; 300]);
    }

    #[test]
    fn delta_of_int32_never_takes_more_than_32_bits() {
        let values: Vec<i32> = (0..256)
            .map(|i| if i % 2 == 0 { i32::MAX } else { i32::MIN })
            .collect();
        let mut out = Vec::new();
        encode_delta_i32(&values, &mut out);
        // Block size, mini-blocks, count (2 bytes), first value (5 bytes) and min delta.
        let mut pos = 0;
        for _ in 0..3 {
            read_uleb128(&out, &mut pos).unwrap();
        }
        read_zigzag(&out, &mut pos).unwrap();
        read_zigzag(&out, &mut pos).unwrap();
        assert!(out[pos..pos + 4].iter().all(|&width| width <= 32));
    }

    fn check_byte_arrays(values: &[&[u8]]) {
        let joined = |offsets: &[u32], bytes: &[u8]| -> Vec<Vec<u8>> {
            offsets
                .windows(2)
                .map(|w| bytes[w[0] as usize..w[1] as usize].to_vec())
                .collect()
        };
        let expected: Vec<Vec<u8>> = values.iter().map(|v| v.to_vec()).collect();

        let mut out = Vec::new();
        encode_delta_length_byte_array(values, &mut out);
        let (mut offsets, mut bytes) = (vec![0u32], Vec::new());
        let (count, used) = decode_delta_length_byte_array(
            &out,
            &mut ByteArrays {
                offsets: &mut offsets,
                bytes: &mut bytes,
            },
        )
        .unwrap();
        assert_eq!((count, used), (values.len(), out.len()));
        assert_eq!(joined(&offsets, &bytes), expected);

        let mut out = Vec::new();
        encode_delta_byte_array(values, &mut out);
        let (mut offsets, mut bytes) = (vec![0u32], Vec::new());
        let (count, used) = decode_delta_byte_array(
            &out,
            &mut ByteArrays {
                offsets: &mut offsets,
                bytes: &mut bytes,
            },
        )
        .unwrap();
        assert_eq!((count, used), (values.len(), out.len()));
        assert_eq!(joined(&offsets, &bytes), expected);
    }

    #[test]
    fn byte_arrays_round_trip() {
        check_byte_arrays(&[]);
        check_byte_arrays(&[b""]);
        check_byte_arrays(&[b"", b"", b"a", b"", b"a"]);
        check_byte_arrays(&[b"Hello", b"World", b"Foobar", b"ABCDEF"]);
        check_byte_arrays(&[
            b"axis",
            b"axle",
            b"babble",
            b"babyhood",
            b"\xff\x00",
            b"\xff\x00z",
        ]);
        let many: Vec<Vec<u8>> = (0..1000)
            .map(|i| format!("https://example.com/{}", i % 37).into_bytes())
            .collect();
        let refs: Vec<&[u8]> = many.iter().map(Vec::as_slice).collect();
        check_byte_arrays(&refs);
    }

    #[test]
    fn delta_byte_array_stores_only_the_suffixes() {
        let mut out = Vec::new();
        encode_delta_byte_array(&[b"axis", b"axle", b"babble", b"babyhood"], &mut out);
        // The spec example: suffixes axis, le, babble, yhood.
        assert!(out.ends_with(b"axislebabbleyhood"));
    }
}
