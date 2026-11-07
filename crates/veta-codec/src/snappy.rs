//! Raw Snappy (no framing), as described by `format_description.txt` of Snappy 1.3.1
//! (contract §8.3). Used only for Parquet pages.
//!
//! A stream is the uncompressed length as a little-endian varint (at most `u32::MAX`)
//! followed by elements whose tag byte's low two bits select a literal (`00`), a copy with
//! a 1-byte offset (`01`: length 4–11, offset < 2048), a 2-byte offset (`10`: length
//! 1–64) or a 4-byte offset (`11`: length 1–64). The compressor is greedy with a hash table
//! of the last position of each 4-byte hash. The decompressor rejects truncated input,
//! zero offsets, offsets past the output written so far and output longer or shorter than
//! the preamble declares.

use veta_core::{Result, VetaError};

const HASH_BITS: u32 = 14;
const EMPTY: u32 = u32::MAX;
const MIN_MATCH: usize = 4;

/// Bytes of output per byte of input are at most 64 / 3 (a 2-byte-offset copy of 64).
const MAX_EXPANSION: usize = 22;

/// Compresses `data` into a raw Snappy stream.
///
/// Panics if `data` is longer than `u32::MAX` bytes, the format's limit.
pub fn compress(data: &[u8]) -> Vec<u8> {
    let n = data.len();
    assert!(
        n <= u32::MAX as usize,
        "snappy input of {n} bytes exceeds u32"
    );
    let mut out = Vec::with_capacity(n + n / 6 + 16);
    put_varint(&mut out, n as u32);
    let mut anchor = 0;
    if n >= MIN_MATCH {
        let mut table = vec![EMPTY; 1 << HASH_BITS];
        let hash = |pos: usize| -> usize {
            (read_u32(data, pos).wrapping_mul(0x1E35_A7BD) >> (32 - HASH_BITS)) as usize
        };
        let mut i = 0;
        // Misses advance faster and faster through incompressible data, as Snappy does.
        let mut skip = 32usize;
        while i + MIN_MATCH <= n {
            let h = hash(i);
            let cand = table[h];
            table[h] = i as u32;
            if cand != EMPTY && read_u32(data, cand as usize) == read_u32(data, i) {
                let c = cand as usize;
                let mut len = MIN_MATCH;
                while i + len < n && data[c + len] == data[i + len] {
                    len += 1;
                }
                put_literal(&mut out, &data[anchor..i]);
                put_copy(&mut out, i - c, len);
                i += len;
                anchor = i;
                // Index the position just before the next search to catch adjacent repeats.
                if i + MIN_MATCH <= n && i >= 1 {
                    table[hash(i - 1)] = (i - 1) as u32;
                }
                skip = 32;
            } else {
                i += skip >> 5;
                skip += 1;
            }
        }
    }
    put_literal(&mut out, &data[anchor..]);
    out
}

fn read_u32(data: &[u8], pos: usize) -> u32 {
    u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap())
}

fn put_varint(out: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn put_literal(out: &mut Vec<u8>, literal: &[u8]) {
    if literal.is_empty() {
        return;
    }
    let n = literal.len() - 1;
    if n < 60 {
        out.push((n as u8) << 2);
    } else {
        let bytes = (32 - (n as u32).leading_zeros()).div_ceil(8) as usize;
        out.push(((59 + bytes) as u8) << 2);
        out.extend_from_slice(&(n as u32).to_le_bytes()[..bytes]);
    }
    out.extend_from_slice(literal);
}

fn put_copy_piece(out: &mut Vec<u8>, offset: usize, len: usize) {
    debug_assert!((1..=64).contains(&len));
    if (4..=11).contains(&len) && offset < 2048 {
        out.push(0b01 | (((len - 4) as u8) << 2) | (((offset >> 8) as u8) << 5));
        out.push(offset as u8);
    } else if offset <= 0xFFFF {
        out.push(0b10 | (((len - 1) as u8) << 2));
        out.extend_from_slice(&(offset as u16).to_le_bytes());
    } else {
        out.push(0b11 | (((len - 1) as u8) << 2));
        out.extend_from_slice(&(offset as u32).to_le_bytes());
    }
}

/// Emits a match of `len >= 4` as copies of at most 64, keeping every piece at least 4
/// long so short pieces can still use the 1-byte-offset form.
fn put_copy(out: &mut Vec<u8>, offset: usize, mut len: usize) {
    while len >= 68 {
        put_copy_piece(out, offset, 64);
        len -= 64;
    }
    if len > 64 {
        put_copy_piece(out, offset, 60);
        len -= 60;
    }
    put_copy_piece(out, offset, len);
}

fn malformed(reason: impl Into<String>) -> VetaError {
    VetaError::corrupt("", format!("snappy: {}", reason.into()))
}

fn get_varint(input: &[u8]) -> Result<(usize, usize)> {
    let mut value = 0u64;
    for (i, &b) in input.iter().enumerate().take(5) {
        value |= ((b & 0x7F) as u64) << (7 * i);
        if b & 0x80 == 0 {
            if value > u32::MAX as u64 {
                return Err(malformed("length preamble above u32"));
            }
            return Ok((value as usize, i + 1));
        }
    }
    Err(malformed("truncated or overlong length preamble"))
}

/// Decompresses a raw Snappy stream. Malformed input is `Err`.
pub fn decompress(input: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    decompress_into(input, &mut out)?;
    Ok(out)
}

/// [`decompress`] into `out`, replacing its contents and reusing its allocation. After an
/// `Err`, the contents of `out` are unspecified.
pub fn decompress_into(input: &[u8], out: &mut Vec<u8>) -> Result<()> {
    let (total, mut ip) = get_varint(input)?;
    if total > (input.len() - ip).saturating_mul(MAX_EXPANSION) {
        return Err(malformed(format!(
            "declared size {total} is impossible for {} bytes",
            input.len()
        )));
    }
    out.clear();
    out.reserve(total);
    while ip < input.len() {
        let tag = input[ip];
        ip += 1;
        let (offset, len) = match tag & 3 {
            0 => {
                let mut len = (tag >> 2) as usize;
                if len >= 60 {
                    let bytes = len - 59;
                    if input.len() - ip < bytes {
                        return Err(malformed("truncated literal length"));
                    }
                    let mut le = [0u8; 4];
                    le[..bytes].copy_from_slice(&input[ip..ip + bytes]);
                    ip += bytes;
                    len = u32::from_le_bytes(le) as usize;
                }
                let len = len + 1;
                if len > input.len() - ip {
                    return Err(malformed("truncated literal"));
                }
                if len > total - out.len() {
                    return Err(malformed("literal past the declared size"));
                }
                out.extend_from_slice(&input[ip..ip + len]);
                ip += len;
                continue;
            }
            1 => {
                let Some(&low) = input.get(ip) else {
                    return Err(malformed("truncated copy"));
                };
                ip += 1;
                let len = ((tag >> 2) & 7) as usize + 4;
                (((tag >> 5) as usize) << 8 | low as usize, len)
            }
            2 => {
                if input.len() - ip < 2 {
                    return Err(malformed("truncated copy"));
                }
                let offset = u16::from_le_bytes([input[ip], input[ip + 1]]) as usize;
                ip += 2;
                (offset, (tag >> 2) as usize + 1)
            }
            _ => {
                if input.len() - ip < 4 {
                    return Err(malformed("truncated copy"));
                }
                let offset = u32::from_le_bytes(input[ip..ip + 4].try_into().unwrap()) as usize;
                ip += 4;
                (offset, (tag >> 2) as usize + 1)
            }
        };
        let pos = out.len();
        if offset == 0 || offset > pos {
            return Err(malformed(format!("offset {offset} at position {pos}")));
        }
        if len > total - pos {
            return Err(malformed("copy past the declared size"));
        }
        let start = pos - offset;
        if offset >= len {
            out.extend_from_within(start..start + len);
        } else {
            for k in 0..len {
                let b = out[start + k];
                out.push(b);
            }
        }
    }
    if out.len() != total {
        return Err(malformed(format!(
            "decoded {} bytes, preamble says {total}",
            out.len()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use veta_core::rand::Rng;

    fn roundtrip(data: &[u8]) -> Vec<u8> {
        let packed = compress(data);
        assert_eq!(decompress(&packed).unwrap(), data);
        packed
    }

    fn random(len: usize, seed: u64) -> Vec<u8> {
        let mut data = vec![0u8; len];
        Rng::new(seed).fill_bytes(&mut data);
        data
    }

    #[test]
    fn decompress_into_replaces_the_contents() {
        let data = b"snappy-snappy-snappy-veta".repeat(40);
        let mut out = b"stale bytes".to_vec();
        decompress_into(&compress(&data), &mut out).unwrap();
        assert_eq!(out, data);
        decompress_into(&compress(b"xy"), &mut out).unwrap();
        assert_eq!(out, b"xy");
    }

    #[test]
    fn empty() {
        assert_eq!(roundtrip(&[]), [0]);
    }

    #[test]
    fn one_byte() {
        assert_eq!(roundtrip(&[42]), [1, 0, 42]);
    }

    #[test]
    fn short_inputs() {
        for len in 0..80 {
            roundtrip(&random(len, len as u64));
            roundtrip(&vec![3u8; len]);
        }
    }

    #[test]
    fn all_equal() {
        let data = vec![0x5Au8; 1 << 20];
        let packed = roundtrip(&data);
        assert!(packed.len() * 15 < data.len());
    }

    #[test]
    fn random_incompressible() {
        let data = random(100_000, 1);
        let packed = roundtrip(&data);
        assert!(packed.len() <= data.len() + 16);
    }

    #[test]
    fn long_literals_use_every_length_width() {
        for len in [60, 61, 256, 257, 65_536, 65_537, 70_000] {
            roundtrip(&random(len, len as u64));
        }
    }

    #[test]
    fn repetitive_text() {
        let text = "l_orderkey|l_partkey|l_suppkey|l_linenumber|l_quantity\n".repeat(2000);
        let packed = roundtrip(text.as_bytes());
        assert!(packed.len() * 10 < text.len());
    }

    #[test]
    fn matches_crossing_64_kib() {
        let block = random(1000, 2);
        let mut data = random(65_000, 3);
        data.extend_from_slice(&block);
        data.extend_from_slice(&random(500, 4));
        data.extend_from_slice(&block);
        roundtrip(&data);
    }

    #[test]
    fn four_byte_offsets() {
        let block = random(2048, 5);
        let mut data = block.clone();
        data.extend_from_slice(&random(100_000, 6));
        data.extend_from_slice(&block);
        roundtrip(&data);
    }

    #[test]
    fn overlapping_matches() {
        for period in 1..=15usize {
            let seed = random(period, period as u64);
            let data: Vec<u8> = (0..5000).map(|i| seed[i % period]).collect();
            let packed = roundtrip(&data);
            assert!(
                packed.len() < 400,
                "period {period}: {} bytes",
                packed.len()
            );
        }
    }

    #[test]
    fn spec_example() {
        // "xababab" = <literal "xab"> <copy offset=2 length=4> (format_description.txt §2.2),
        // the copy with a 1-byte offset: tag 0b000_000_01, offset byte 2.
        let stream = [0x07, 0x08, b'x', b'a', b'b', 0x01, 0x02];
        assert_eq!(decompress(&stream).unwrap(), b"xababab");
    }

    #[test]
    fn hand_built_copies_of_every_width() {
        // "abcd", then the same 4 bytes as a 2-byte-offset copy and a 4-byte-offset copy.
        let stream = [
            12, 0x0C, b'a', b'b', b'c', b'd', 0x0E, 4, 0, 0x0F, 8, 0, 0, 0,
        ];
        assert_eq!(decompress(&stream).unwrap(), b"abcdabcdabcd");
        // Literal whose length is in one extra byte (tag 60): 61 bytes.
        let mut long = vec![61, 60 << 2, 60];
        long.extend(std::iter::repeat_n(b'z', 61));
        assert_eq!(decompress(&long).unwrap(), vec![b'z'; 61]);
    }

    #[test]
    fn varint_preamble() {
        let mut out = Vec::new();
        put_varint(&mut out, 2_097_150);
        assert_eq!(out, [0xFE, 0xFF, 0x7F]);
        assert_eq!(get_varint(&out).unwrap(), (2_097_150, 3));
        assert_eq!(get_varint(&[0x40]).unwrap(), (64, 1));
        assert!(get_varint(&[0xFF, 0xFF, 0xFF, 0xFF, 0x7F]).is_err());
        assert!(get_varint(&[0x80, 0x80]).is_err());
    }

    #[test]
    fn truncated_inputs_are_errors() {
        let data = "the quick brown fox jumps over the lazy dog ".repeat(50);
        let packed = compress(data.as_bytes());
        for cut in 0..packed.len() {
            assert!(decompress(&packed[..cut]).is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn impossible_offsets_are_errors() {
        // Offset 0.
        assert!(decompress(&[7, 0x08, b'x', b'a', b'b', 0x01, 0x00]).is_err());
        // Offset 4 with only 3 bytes written.
        assert!(decompress(&[7, 0x08, b'x', b'a', b'b', 0x01, 0x04]).is_err());
        // A copy first.
        assert!(decompress(&[4, 0x01, 0x01]).is_err());
        // Copy past the declared size.
        assert!(decompress(&[6, 0x08, b'x', b'a', b'b', 0x01, 0x02]).is_err());
        // Output shorter than declared.
        assert!(decompress(&[8, 0x08, b'x', b'a', b'b', 0x01, 0x02]).is_err());
        // Declared size impossible for the input.
        assert!(decompress(&[0xFF, 0xFF, 0xFF, 0xFF, 0x0F, 0x00, 0]).is_err());
    }

    #[test]
    fn corrupted_streams_never_panic() {
        let data = "abcabcabd 0123456789 abcabcabd ".repeat(200);
        let packed = compress(data.as_bytes());
        let mut rng = Rng::new(11);
        for _ in 0..2000 {
            let mut bad = packed.clone();
            for _ in 0..1 + rng.below(4) {
                let at = rng.below(bad.len() as u64) as usize;
                bad[at] = rng.next_u32() as u8;
            }
            let _ = decompress(&bad);
        }
    }
}
