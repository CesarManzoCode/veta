//! `vlz`, the general-purpose LZ77 compressor of block payloads (contract §8.2).
//!
//! Format (all integers little-endian):
//!
//! ```text
//! u32 decompressed length
//! sequence*:
//!   token u8           high nibble: literal count (15 = extended)
//!                      low nibble: match length − 4 (15 = extended)
//!   literal extension  bytes added to 15 while they are 255; the first byte < 255 ends it
//!   literals
//!   offset             3 bytes, 1..=2^24 − 1 back from the current output position
//!   match extension    as the literal extension, added to 15 + 4
//! ```
//!
//! The last sequence carries only the token, its literal extension and the literals; it
//! ends exactly when the output reaches the decompressed length, and it is always present
//! (an empty input compresses to the header and a zero token). The match-length extension
//! follows the offset, so a reader knows whether a match follows before reading it.
//!
//! The compressor indexes every position with a hash of its next 4 bytes and walks hash
//! chains up to [`CHAIN_DEPTH`] candidates deep, then tries the next position once (lazy
//! matching) before emitting. The decompressor never reads or writes out of range: any
//! malformed input (truncated, trailing bytes, zero or too-distant offsets, lengths past
//! the declared size) is `Err`.

use veta_core::{Result, VetaError};

/// Shortest match the format encodes.
pub const MIN_MATCH: usize = 4;

/// Largest offset (3 bytes).
pub const MAX_OFFSET: usize = (1 << 24) - 1;

/// Hash chain candidates examined per position.
pub const CHAIN_DEPTH: usize = 16;

const HASH_BITS: u32 = 16;
const EMPTY: u32 = u32::MAX;

/// Output slack past the decompressed length, so short literals and long-offset matches
/// can be copied in whole 16-byte chunks.
const SLACK: usize = 32;

/// Upper bound on bytes of output per byte of input: one extension byte of 255 adds 255
/// bytes, and every other construct expands less.
const MAX_EXPANSION: usize = 255;

/// Whether a compressed payload is worth storing instead of the raw one: it must save at
/// least 5 % (contract §8.2).
pub fn pays_off(raw_len: usize, compressed_len: usize) -> bool {
    (compressed_len as u128) * 100 <= (raw_len as u128) * 95
}

/// Compresses `data` into the `vlz` format.
///
/// Panics if `data` is longer than `u32::MAX` bytes, the largest length the header holds
/// (block payloads are far smaller).
pub fn compress(data: &[u8]) -> Vec<u8> {
    let n = data.len();
    assert!(n <= u32::MAX as usize, "vlz input of {n} bytes exceeds u32");
    let mut out = Vec::with_capacity(n + n / 255 + 16);
    out.extend_from_slice(&(n as u32).to_le_bytes());
    let mut anchor = 0;
    if n >= MIN_MATCH {
        let mut finder = MatchFinder::new(data);
        let mut i = 0;
        while i + MIN_MATCH <= n {
            let (mut len, mut offset) = finder.best(i);
            if len < MIN_MATCH {
                i += 1;
                continue;
            }
            if i + 1 + MIN_MATCH <= n {
                let (len2, offset2) = finder.best(i + 1);
                if len2 > len {
                    i += 1;
                    len = len2;
                    offset = offset2;
                }
            }
            put_sequence(&mut out, &data[anchor..i], offset, len);
            i += len;
            anchor = i;
        }
    }
    put_last(&mut out, &data[anchor..]);
    out
}

/// Hash chains over every position of the input (contract §8.2).
struct MatchFinder<'a> {
    data: &'a [u8],
    head: Vec<u32>,
    chain: Vec<u32>,
    mask: usize,
    /// Next position not yet inserted.
    next: usize,
}

impl<'a> MatchFinder<'a> {
    fn new(data: &'a [u8]) -> Self {
        // The ring covers the whole window, so a live chain entry is never overwritten.
        let size = data.len().next_power_of_two().min(MAX_OFFSET + 1);
        MatchFinder {
            data,
            head: vec![EMPTY; 1 << HASH_BITS],
            chain: vec![EMPTY; size],
            mask: size - 1,
            next: 0,
        }
    }

    fn hash(&self, pos: usize) -> usize {
        (read_u32(self.data, pos).wrapping_mul(2_654_435_761) >> (32 - HASH_BITS)) as usize
    }

    fn insert(&mut self, pos: usize) {
        let h = self.hash(pos);
        self.chain[pos & self.mask] = self.head[h];
        self.head[h] = pos as u32;
    }

    /// Longest match for `pos` among up to [`CHAIN_DEPTH`] earlier candidates as
    /// `(length, offset)`; length 0 if none. Inserts every position up to `pos`.
    fn best(&mut self, pos: usize) -> (usize, usize) {
        let last = self.data.len() - MIN_MATCH;
        while self.next < pos {
            if self.next <= last {
                self.insert(self.next);
            }
            self.next += 1;
        }
        let (mut best_len, mut best_off) = (0, 0);
        let mut cand = self.head[self.hash(pos)];
        let mut depth = CHAIN_DEPTH;
        while cand != EMPTY && depth > 0 {
            let c = cand as usize;
            if c >= pos || pos - c > MAX_OFFSET {
                break;
            }
            // Cheap reject: a better match must also agree on the byte at `best_len`.
            if pos + best_len < self.data.len()
                && self.data[c + best_len] == self.data[pos + best_len]
            {
                let len = match_len(self.data, c, pos);
                if len > best_len {
                    best_len = len;
                    best_off = pos - c;
                    if pos + len == self.data.len() {
                        break;
                    }
                }
            }
            let next = self.chain[c & self.mask];
            if next != EMPTY && next as usize >= c {
                break;
            }
            cand = next;
            depth -= 1;
        }
        if self.next == pos {
            if pos <= last {
                self.insert(pos);
            }
            self.next = pos + 1;
        }
        (best_len, best_off)
    }
}

fn read_u32(data: &[u8], pos: usize) -> u32 {
    u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap())
}

fn read_u64(data: &[u8], pos: usize) -> u64 {
    u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap())
}

/// Length of the common run of `data[a..]` and `data[b..]`, with `a < b`.
fn match_len(data: &[u8], a: usize, b: usize) -> usize {
    let n = data.len();
    let mut len = 0;
    while b + len + 8 <= n {
        let x = read_u64(data, a + len) ^ read_u64(data, b + len);
        if x != 0 {
            return len + (x.trailing_zeros() / 8) as usize;
        }
        len += 8;
    }
    while b + len < n && data[a + len] == data[b + len] {
        len += 1;
    }
    len
}

fn put_extension(out: &mut Vec<u8>, mut rest: usize) {
    while rest >= 255 {
        out.push(255);
        rest -= 255;
    }
    out.push(rest as u8);
}

fn put_sequence(out: &mut Vec<u8>, literals: &[u8], offset: usize, len: usize) {
    debug_assert!((1..=MAX_OFFSET).contains(&offset) && len >= MIN_MATCH);
    let lit = literals.len();
    let m = len - MIN_MATCH;
    out.push(((lit.min(15) as u8) << 4) | m.min(15) as u8);
    if lit >= 15 {
        put_extension(out, lit - 15);
    }
    out.extend_from_slice(literals);
    out.extend_from_slice(&(offset as u32).to_le_bytes()[..3]);
    if m >= 15 {
        put_extension(out, m - 15);
    }
}

fn put_last(out: &mut Vec<u8>, literals: &[u8]) {
    let lit = literals.len();
    out.push((lit.min(15) as u8) << 4);
    if lit >= 15 {
        put_extension(out, lit - 15);
    }
    out.extend_from_slice(literals);
}

fn malformed(reason: impl Into<String>) -> VetaError {
    VetaError::corrupt("", format!("vlz: {}", reason.into()))
}

/// Reads an extension: bytes summed while they are 255.
#[inline(always)]
fn get_extension(input: &[u8], ip: &mut usize, limit: usize) -> Result<usize> {
    let mut sum = 0usize;
    loop {
        let Some(&b) = input.get(*ip) else {
            return Err(malformed("truncated length extension"));
        };
        *ip += 1;
        sum += b as usize;
        if sum > limit {
            return Err(malformed("length past the decompressed size"));
        }
        if b != 255 {
            return Ok(sum);
        }
    }
}

/// Decompresses a `vlz` payload. Malformed input is `Err` (contract §6.4).
pub fn decompress(input: &[u8]) -> Result<Vec<u8>> {
    if input.len() < 4 {
        return Err(malformed("missing header"));
    }
    let total = u32::from_le_bytes(input[..4].try_into().unwrap()) as usize;
    if total > (input.len() - 4).saturating_mul(MAX_EXPANSION) {
        return Err(malformed(format!(
            "declared size {total} is impossible for {} bytes",
            input.len()
        )));
    }
    let mut out: Vec<u8> = Vec::with_capacity(total + SLACK);
    let dst = out.as_mut_ptr();
    let mut ip = 4;
    let mut op = 0usize;
    loop {
        let Some(&token) = input.get(ip) else {
            return Err(malformed("truncated before a token"));
        };
        ip += 1;

        let mut lit = (token >> 4) as usize;
        if lit == 15 {
            lit += get_extension(input, &mut ip, total)?;
        }
        if lit > total - op {
            return Err(malformed("literals past the decompressed size"));
        }
        if lit > input.len() - ip {
            return Err(malformed("truncated literals"));
        }
        if lit <= 16 && input.len() - ip >= 16 {
            // SAFETY: 16 bytes are readable at `ip` (checked above) and writable at `op`,
            // since `op + lit <= total` and `lit <= 16` give `op + 16 <= total + SLACK`.
            unsafe { std::ptr::copy_nonoverlapping(input.as_ptr().add(ip), dst.add(op), 16) };
        } else {
            // SAFETY: `ip + lit <= input.len()` and `op + lit <= total < capacity`, both
            // checked above; input and output never overlap.
            unsafe { std::ptr::copy_nonoverlapping(input.as_ptr().add(ip), dst.add(op), lit) };
        }
        ip += lit;
        op += lit;

        if op == total {
            if ip != input.len() {
                return Err(malformed("trailing bytes after the last sequence"));
            }
            break;
        }

        if input.len() - ip < 3 {
            return Err(malformed("truncated offset"));
        }
        let offset =
            input[ip] as usize | (input[ip + 1] as usize) << 8 | (input[ip + 2] as usize) << 16;
        ip += 3;
        if offset == 0 || offset > op {
            return Err(malformed(format!("offset {offset} at position {op}")));
        }
        let mut len = (token & 15) as usize + MIN_MATCH;
        if len == 15 + MIN_MATCH {
            len += get_extension(input, &mut ip, total)?;
        }
        if len > total - op {
            return Err(malformed("match past the decompressed size"));
        }

        if offset >= 16 {
            let mut done = 0;
            while done < len {
                // SAFETY: the source chunk `[op − offset + done, +16)` ends at or before
                // `op + done` because `offset >= 16`, so it was already written and does
                // not overlap the destination chunk; the destination ends below
                // `op + len + 16 <= total + 16 < capacity`.
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        dst.add(op - offset + done),
                        dst.add(op + done),
                        16,
                    )
                };
                done += 16;
            }
        } else {
            for k in 0..len {
                // SAFETY: `op + k < total <= capacity` and the source `op + k − offset` is
                // below `op + k`, so it was written before (byte by byte for overlaps).
                unsafe { *dst.add(op + k) = *dst.add(op + k - offset) };
            }
        }
        op += len;
    }
    // SAFETY: every byte in `[0, total)` was written above (`op == total`).
    unsafe { out.set_len(total) };
    Ok(out)
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
    fn empty() {
        let packed = roundtrip(&[]);
        assert_eq!(packed, [0, 0, 0, 0, 0]);
    }

    #[test]
    fn one_byte() {
        let packed = roundtrip(&[42]);
        assert_eq!(packed, [1, 0, 0, 0, 0x10, 42]);
    }

    #[test]
    fn short_inputs() {
        for len in 0..64 {
            roundtrip(&random(len, len as u64));
            roundtrip(&vec![7u8; len]);
        }
    }

    #[test]
    fn all_equal() {
        let data = vec![0xABu8; 1 << 20];
        let packed = roundtrip(&data);
        assert!(packed.len() < 8192, "{} bytes", packed.len());
    }

    #[test]
    fn random_incompressible() {
        let data = random(100_000, 1);
        let packed = roundtrip(&data);
        assert!(packed.len() <= data.len() + data.len() / 255 + 16);
        assert!(!pays_off(data.len(), packed.len()));
    }

    #[test]
    fn repetitive_text() {
        let text = "SELECT count(*) FROM hits WHERE URL LIKE '%google%';\n".repeat(2000);
        let packed = roundtrip(text.as_bytes());
        assert!(packed.len() * 20 < text.len());
        assert!(pays_off(text.len(), packed.len()));
    }

    #[test]
    fn matches_crossing_64_kib() {
        let block = random(1000, 2);
        let mut data = random(65_000, 3);
        data.extend_from_slice(&block);
        data.extend_from_slice(&random(500, 4));
        data.extend_from_slice(&block);
        data.extend_from_slice(&block);
        roundtrip(&data);
    }

    #[test]
    fn offsets_above_65535() {
        let block = random(4096, 5);
        let mut data = block.clone();
        data.extend_from_slice(&random(200_000, 6));
        data.extend_from_slice(&block);
        let packed = roundtrip(&data);
        // The second copy of the block is a single long-distance match.
        assert!(packed.len() < data.len() - 3000);
    }

    #[test]
    fn overlapping_matches() {
        for period in 1..=15usize {
            let seed = random(period, period as u64);
            let data: Vec<u8> = (0..5000).map(|i| seed[i % period]).collect();
            let packed = roundtrip(&data);
            assert!(
                packed.len() < 200,
                "period {period}: {} bytes",
                packed.len()
            );
        }
    }

    #[test]
    fn mixed_random_structures() {
        let mut rng = Rng::new(9);
        for _ in 0..50 {
            let mut data = Vec::new();
            for _ in 0..rng.below(40) {
                match rng.below(3) {
                    0 => data.extend_from_slice(&random(rng.below(300) as usize, rng.next_u64())),
                    1 => data.extend(std::iter::repeat_n(
                        rng.below(4) as u8,
                        rng.below(500) as usize,
                    )),
                    _ if !data.is_empty() => {
                        let start = rng.below(data.len() as u64) as usize;
                        let len = rng.below((data.len() - start) as u64 + 1) as usize;
                        let piece = data[start..start + len].to_vec();
                        data.extend_from_slice(&piece);
                    }
                    _ => {}
                }
            }
            roundtrip(&data);
        }
    }

    #[test]
    fn hand_built_stream() {
        // "abcd" literal, then a match of 8 at offset 4, then the empty last sequence.
        let stream = [12, 0, 0, 0, 0x44, b'a', b'b', b'c', b'd', 4, 0, 0, 0x00];
        assert_eq!(decompress(&stream).unwrap(), b"abcdabcdabcd");
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
    fn trailing_bytes_are_errors() {
        let mut packed = compress(b"hello hello hello");
        packed.push(0);
        assert!(decompress(&packed).is_err());
    }

    #[test]
    fn impossible_offsets_are_errors() {
        // Offset 0.
        let zero = [8, 0, 0, 0, 0x40, b'a', b'b', b'c', b'd', 0, 0, 0, 0x00];
        assert!(decompress(&zero).is_err());
        // Offset 5 with only 4 bytes written.
        let far = [8, 0, 0, 0, 0x40, b'a', b'b', b'c', b'd', 5, 0, 0, 0x00];
        assert!(decompress(&far).is_err());
        // Match longer than the declared size.
        let long = [8, 0, 0, 0, 0x41, b'a', b'b', b'c', b'd', 4, 0, 0, 0x00];
        assert!(decompress(&long).is_err());
        // A copy as the very first thing.
        assert!(decompress(&[4, 0, 0, 0, 0x00, 1, 0, 0, 0x00]).is_err());
    }

    #[test]
    fn impossible_sizes_are_errors() {
        assert!(decompress(&[0xFF, 0xFF, 0xFF, 0xFF, 0x00]).is_err());
        assert!(decompress(&[]).is_err());
        assert!(decompress(&[1, 0, 0]).is_err());
        // Declared 2 bytes, only one literal and no more input.
        assert!(decompress(&[2, 0, 0, 0, 0x10, 1]).is_err());
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
            if let Ok(out) = decompress(&bad) {
                assert_eq!(
                    out.len() as u32,
                    u32::from_le_bytes(bad[..4].try_into().unwrap())
                );
            }
        }
    }

    #[test]
    fn pays_off_threshold() {
        assert!(pays_off(100, 95));
        assert!(!pays_off(100, 96));
        assert!(pays_off(0, 0));
    }
}
