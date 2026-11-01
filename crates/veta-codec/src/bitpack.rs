//! Bit packing of `u64` values with a fixed width of 0–64 bits (contract §8.1).
//!
//! Values are packed LSB-first into little-endian 64-bit words, in mini-blocks of
//! [`MINI_BLOCK`] values. A full mini-block of width `w` fills exactly `2·w` words, so the
//! mini-blocks laid end to end form one continuous bit stream: value `i` starts at bit
//! `i·w`. The last mini-block may be partial and takes `ceil(n·w / 64)` words. Width 0
//! takes no bytes at all.

use veta_core::{Result, VetaError};

/// Values per mini-block.
pub const MINI_BLOCK: usize = 128;

/// Largest supported bit width.
pub const MAX_WIDTH: u8 = 64;

/// Smallest width that holds `max` (0 for `max == 0`).
pub fn width_for(max: u64) -> u8 {
    (64 - max.leading_zeros()) as u8
}

fn mask(width: u8) -> u64 {
    if width >= 64 {
        u64::MAX
    } else {
        (1u64 << width) - 1
    }
}

fn invalid_width(width: u8) -> VetaError {
    VetaError::corrupt("", format!("bit width {width} is larger than {MAX_WIDTH}"))
}

/// Bytes taken by `count` values of `width` bits. Errors on an invalid width or overflow.
pub fn packed_len(count: usize, width: u8) -> Result<usize> {
    if width > MAX_WIDTH {
        return Err(invalid_width(width));
    }
    let bits = (count as u128) * (width as u128);
    let bytes = bits.div_ceil(64) * 8;
    usize::try_from(bytes)
        .map_err(|_| VetaError::corrupt("", format!("{count} packed values overflow")))
}

/// Appends `values` packed at `width` bits. Bits of a value above `width` are dropped.
///
/// Panics if `width > 64` (a caller bug: the width always comes from [`width_for`]).
pub fn pack(values: &[u64], width: u8, out: &mut Vec<u8>) {
    assert!(width <= MAX_WIDTH, "bit width {width} > {MAX_WIDTH}");
    if width == 0 || values.is_empty() {
        return;
    }
    let w = width as usize;
    let m = mask(width);
    for block in values.chunks(MINI_BLOCK) {
        let mut words = vec![0u64; (block.len() * w).div_ceil(64)];
        for (i, &value) in block.iter().enumerate() {
            let v = value & m;
            let bit = i * w;
            let (idx, shift) = (bit / 64, bit % 64);
            words[idx] |= v << shift;
            if shift + w > 64 {
                words[idx + 1] |= v >> (64 - shift);
            }
        }
        for word in words {
            out.extend_from_slice(&word.to_le_bytes());
        }
    }
}

/// Unpacks `count` values of `width` bits from the start of `bytes`. Errors if the width is
/// invalid or `bytes` is shorter than [`packed_len`]; extra bytes are ignored.
pub fn unpack(bytes: &[u8], count: usize, width: u8) -> Result<Vec<u64>> {
    let needed = packed_len(count, width)?;
    if bytes.len() < needed {
        return Err(VetaError::corrupt(
            "",
            format!(
                "bit-packed data truncated: {} bytes, need {needed}",
                bytes.len()
            ),
        ));
    }
    if width == 0 {
        return Ok(vec![0; count]);
    }
    let words: Vec<u64> = bytes[..needed]
        .chunks_exact(8)
        .map(|c| u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]))
        .collect();
    let w = width as usize;
    let m = mask(width);
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let bit = i * w;
        let (idx, shift) = (bit / 64, bit % 64);
        let mut v = words[idx] >> shift;
        if shift + w > 64 {
            v |= words[idx + 1] << (64 - shift);
        }
        out.push(v & m);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use veta_core::rand::Rng;

    fn roundtrip(values: &[u64], width: u8) {
        let mut out = Vec::new();
        pack(values, width, &mut out);
        assert_eq!(out.len(), packed_len(values.len(), width).unwrap());
        assert_eq!(unpack(&out, values.len(), width).unwrap(), values);
    }

    #[test]
    fn width_for_values() {
        assert_eq!(width_for(0), 0);
        assert_eq!(width_for(1), 1);
        assert_eq!(width_for(2), 2);
        assert_eq!(width_for(255), 8);
        assert_eq!(width_for(1 << 62), 63);
        assert_eq!(width_for(u64::MAX), 64);
    }

    #[test]
    fn roundtrip_every_width_and_length() {
        let mut rng = Rng::new(0xB17);
        let lengths = [0usize, 1, 2, 63, 64, 127, 128, 129, 255, 256, 257, 1000];
        for width in 0..=64u8 {
            for &n in &lengths {
                let values: Vec<u64> = (0..n).map(|_| rng.next_u64() & mask(width)).collect();
                roundtrip(&values, width);
            }
        }
    }

    #[test]
    fn extreme_values_at_extreme_widths() {
        for width in [0u8, 1, 63, 64] {
            let top = mask(width);
            let values: Vec<u64> = (0..300).map(|i| if i % 3 == 0 { top } else { 0 }).collect();
            roundtrip(&values, width);
        }
    }

    #[test]
    fn layout_is_lsb_first_in_le_words() {
        let mut out = Vec::new();
        pack(&[1, 0, 3, 2], 2, &mut out);
        // 1 | 0<<2 | 3<<4 | 2<<6 = 0b1011_0001
        assert_eq!(out, vec![0b1011_0001, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn full_mini_block_takes_two_words_per_bit() {
        for width in [1u8, 7, 33, 64] {
            assert_eq!(
                packed_len(MINI_BLOCK, width).unwrap(),
                2 * width as usize * 8
            );
        }
        assert_eq!(packed_len(129, 1).unwrap(), 24);
        assert_eq!(packed_len(1_000_000, 0).unwrap(), 0);
    }

    #[test]
    fn high_bits_are_masked() {
        let mut out = Vec::new();
        pack(&[0xFF], 4, &mut out);
        assert_eq!(unpack(&out, 1, 4).unwrap(), vec![0xF]);
    }

    #[test]
    fn invalid_width_is_error() {
        assert!(packed_len(1, 65).is_err());
        assert!(unpack(&[0; 64], 1, 65).is_err());
        assert!(unpack(&[], 0, 200).is_err());
    }

    #[test]
    fn truncated_input_is_error() {
        let mut out = Vec::new();
        pack(&[5; 200], 3, &mut out);
        assert!(unpack(&out[..out.len() - 1], 200, 3).is_err());
        assert!(unpack(&[], 1, 1).is_err());
    }

    #[test]
    fn huge_count_overflow_is_error() {
        assert!(unpack(&[], usize::MAX, 64).is_err());
    }
}
