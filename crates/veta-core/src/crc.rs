//! CRC32C (Castagnoli) for blocks and checksums (contract §5) and CRC-32 (IEEE) for
//! Snappy framing and other formats.
//!
//! CRC32C uses the SSE4.2 `crc32` instruction when the CPU has it (detected at run time)
//! and a table otherwise; both give the same result.

const CRC32C_POLY: u32 = 0x82F6_3B78;
const CRC32_POLY: u32 = 0xEDB8_8320;

const fn make_table(poly: u32) -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ poly } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

static CRC32C_TABLE: [u32; 256] = make_table(CRC32C_POLY);
static CRC32_TABLE: [u32; 256] = make_table(CRC32_POLY);

fn update_table(table: &[u32; 256], mut state: u32, data: &[u8]) -> u32 {
    for &b in data {
        state = table[((state ^ b as u32) & 0xFF) as usize] ^ (state >> 8);
    }
    state
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "sse4.2")]
fn update_sse42(state: u32, data: &[u8]) -> u32 {
    use std::arch::x86_64::{_mm_crc32_u8, _mm_crc32_u64};
    let mut chunks = data.chunks_exact(8);
    let mut wide = state as u64;
    for c in &mut chunks {
        wide = _mm_crc32_u64(wide, u64::from_le_bytes(c.try_into().expect("8 bytes")));
    }
    let mut state = wide as u32;
    for &b in chunks.remainder() {
        state = _mm_crc32_u8(state, b);
    }
    state
}

fn crc32c_update(state: u32, data: &[u8]) -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        if std::is_x86_feature_detected!("sse4.2") {
            // SAFETY: the CPU supports SSE4.2, checked just above at run time.
            return unsafe { update_sse42(state, data) };
        }
    }
    update_table(&CRC32C_TABLE, state, data)
}

/// CRC32C of `data`.
pub fn crc32c(data: &[u8]) -> u32 {
    crc32c_append(0, data)
}

/// Extends a finished CRC32C `crc` with more data: `crc32c_append(crc32c(a), b)` equals
/// the CRC32C of `a` followed by `b`.
pub fn crc32c_append(crc: u32, data: &[u8]) -> u32 {
    !crc32c_update(!crc, data)
}

/// CRC32C by table only, regardless of the CPU.
pub fn crc32c_software(data: &[u8]) -> u32 {
    !update_table(&CRC32C_TABLE, !0, data)
}

/// CRC-32 (IEEE 802.3, as in zlib) of `data`.
pub fn crc32(data: &[u8]) -> u32 {
    crc32_append(0, data)
}

/// Extends a finished CRC-32 with more data.
pub fn crc32_append(crc: u32, data: &[u8]) -> u32 {
    !update_table(&CRC32_TABLE, !crc, data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rand::Rng;

    #[test]
    fn known_vectors() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
        assert_eq!(crc32c_software(b"123456789"), 0xE306_9283);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32c(b""), 0);
        assert_eq!(crc32(b""), 0);
        // RFC 3720 (iSCSI) B.4.
        assert_eq!(crc32c(&[0u8; 32]), 0x8A91_36AA);
        assert_eq!(crc32c(&[0xFFu8; 32]), 0x62A8_AB43);
        let ascending: Vec<u8> = (0..32u8).collect();
        assert_eq!(crc32c(&ascending), 0x46DD_794E);
    }

    #[test]
    fn hardware_matches_table_on_every_length() {
        let mut rng = Rng::new(7);
        let data: Vec<u8> = (0..1000).map(|_| rng.next_u64() as u8).collect();
        for n in 0..data.len() {
            assert_eq!(crc32c(&data[..n]), crc32c_software(&data[..n]), "len {n}");
        }
    }

    #[test]
    fn append_equals_whole() {
        let data = b"The quick brown fox jumps over the lazy dog";
        for split in 0..data.len() {
            let (a, b) = data.split_at(split);
            assert_eq!(crc32c_append(crc32c(a), b), crc32c(data));
            assert_eq!(crc32_append(crc32(a), b), crc32(data));
        }
        assert_eq!(crc32(data), 0x414F_A339);
    }
}
