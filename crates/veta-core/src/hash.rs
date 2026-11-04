//! Hashing: xxh64 for persisted HyperLogLog registers and a fast 64-bit hash for in-memory
//! hash tables (contract §12).

use std::hash::{BuildHasher, Hasher};

use crate::types::MAX_DECIMAL64_PRECISION;
use crate::value::Value;

const P1: u64 = 0x9E37_79B1_85EB_CA87;
const P2: u64 = 0xC2B2_AE3D_27D4_EB4F;
const P3: u64 = 0x1656_67B1_9E37_79F9;
const P4: u64 = 0x85EB_CA77_C2B2_AE63;
const P5: u64 = 0x27D4_EB2F_1656_67C5;

fn read_u64(data: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(data[at..at + 8].try_into().expect("8 bytes"))
}

fn read_u32(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(data[at..at + 4].try_into().expect("4 bytes"))
}

fn round(acc: u64, input: u64) -> u64 {
    acc.wrapping_add(input.wrapping_mul(P2))
        .rotate_left(31)
        .wrapping_mul(P1)
}

fn merge_round(acc: u64, value: u64) -> u64 {
    (acc ^ round(0, value)).wrapping_mul(P1).wrapping_add(P4)
}

/// XXH64 of `data` with `seed` (reference algorithm, little-endian reads).
pub fn xxh64(data: &[u8], seed: u64) -> u64 {
    let len = data.len();
    let mut p = 0;
    let mut h = if len >= 32 {
        let mut v1 = seed.wrapping_add(P1).wrapping_add(P2);
        let mut v2 = seed.wrapping_add(P2);
        let mut v3 = seed;
        let mut v4 = seed.wrapping_sub(P1);
        while p + 32 <= len {
            v1 = round(v1, read_u64(data, p));
            v2 = round(v2, read_u64(data, p + 8));
            v3 = round(v3, read_u64(data, p + 16));
            v4 = round(v4, read_u64(data, p + 24));
            p += 32;
        }
        let mut h = v1
            .rotate_left(1)
            .wrapping_add(v2.rotate_left(7))
            .wrapping_add(v3.rotate_left(12))
            .wrapping_add(v4.rotate_left(18));
        h = merge_round(h, v1);
        h = merge_round(h, v2);
        h = merge_round(h, v3);
        merge_round(h, v4)
    } else {
        seed.wrapping_add(P5)
    };
    h = h.wrapping_add(len as u64);
    while p + 8 <= len {
        h ^= round(0, read_u64(data, p));
        h = h.rotate_left(27).wrapping_mul(P1).wrapping_add(P4);
        p += 8;
    }
    if p + 4 <= len {
        h ^= (read_u32(data, p) as u64).wrapping_mul(P1);
        h = h.rotate_left(23).wrapping_mul(P2).wrapping_add(P3);
        p += 4;
    }
    while p < len {
        h ^= (data[p] as u64).wrapping_mul(P5);
        h = h.rotate_left(11).wrapping_mul(P1);
        p += 1;
    }
    h ^= h >> 33;
    h = h.wrapping_mul(P2);
    h ^= h >> 29;
    h = h.wrapping_mul(P3);
    h ^ (h >> 32)
}

/// Canonical bytes of a non-NULL value for persisted hashes (contract §12): integers,
/// dates, timestamps and booleans as `i64` LE, `Int128` and decimals with precision > 18
/// as `i128` LE, narrower decimals as `i64` LE, floats as their `f64` bits LE (`-0.0` as
/// `0.0`, every NaN as one NaN) and strings as their bytes.
pub fn canonical_bytes(value: &Value, out: &mut Vec<u8>) -> bool {
    match value {
        Value::Null => return false,
        Value::Boolean(b) => out.extend_from_slice(&(*b as i64).to_le_bytes()),
        Value::Int16(v) => out.extend_from_slice(&(*v as i64).to_le_bytes()),
        Value::Int32(v) => out.extend_from_slice(&(*v as i64).to_le_bytes()),
        Value::Int64(v) => out.extend_from_slice(&v.to_le_bytes()),
        Value::Int128(v) => out.extend_from_slice(&v.to_le_bytes()),
        Value::Float64(f) => {
            let f = if f.is_nan() {
                f64::NAN
            } else if *f == 0.0 {
                0.0
            } else {
                *f
            };
            out.extend_from_slice(&f.to_bits().to_le_bytes())
        }
        Value::Decimal {
            value, precision, ..
        } => {
            if *precision <= MAX_DECIMAL64_PRECISION {
                out.extend_from_slice(&(*value as i64).to_le_bytes())
            } else {
                out.extend_from_slice(&value.to_le_bytes())
            }
        }
        Value::Varchar(b) => out.extend_from_slice(b),
        Value::Date(d) => out.extend_from_slice(&(*d as i64).to_le_bytes()),
        Value::Timestamp(t) => out.extend_from_slice(&t.to_le_bytes()),
    }
    true
}

/// xxh64 (seed 0) of the canonical bytes of a value; `None` for NULL.
pub fn xxh64_value(value: &Value) -> Option<u64> {
    match value {
        Value::Varchar(b) => Some(xxh64(b, 0)),
        _ => {
            let mut buf = Vec::with_capacity(16);
            canonical_bytes(value, &mut buf).then(|| xxh64(&buf, 0))
        }
    }
}

const K0: u64 = 0xA076_1D64_78BD_642F;
const K1: u64 = 0xE703_7ED1_A0B4_28DB;
const K2: u64 = 0x8EBC_6AF0_9C88_C6E3;

/// 64×64 → 128-bit multiply folded with XOR.
#[inline]
fn fold_mul(a: u64, b: u64) -> u64 {
    let r = (a as u128).wrapping_mul(b as u128);
    (r as u64) ^ ((r >> 64) as u64)
}

/// Fast hash of a fixed-width key (not persisted; quality good enough for hash tables).
#[inline]
pub fn hash_u64(x: u64) -> u64 {
    fold_mul(x ^ K0, K1)
}

/// Fast hash of a 128-bit key.
#[inline]
pub fn hash_u128(x: u128) -> u64 {
    fold_mul((x as u64) ^ K0, ((x >> 64) as u64) ^ K1)
}

/// Fast hash of a byte string; the length is mixed in, so `"a"` and `"a\0"` differ.
pub fn hash_bytes(data: &[u8]) -> u64 {
    let mut h = K2 ^ (data.len() as u64).wrapping_mul(K0);
    let mut chunks = data.chunks_exact(16);
    for c in &mut chunks {
        let a = u64::from_le_bytes(c[..8].try_into().expect("8 bytes"));
        let b = u64::from_le_bytes(c[8..].try_into().expect("8 bytes"));
        h = fold_mul(a ^ K0 ^ h, b ^ K1);
    }
    let rest = chunks.remainder();
    if !rest.is_empty() {
        let mut buf = [0u8; 16];
        buf[..rest.len()].copy_from_slice(rest);
        let a = u64::from_le_bytes(buf[..8].try_into().expect("8 bytes"));
        let b = u64::from_le_bytes(buf[8..].try_into().expect("8 bytes"));
        h = fold_mul(a ^ K0 ^ h, b ^ K1);
    }
    fold_mul(h, K2)
}

/// [`Hasher`] built on the fast hash, for `HashMap`/`HashSet` keyed by engine values.
#[derive(Debug, Clone, Copy, Default)]
pub struct FastHasher {
    state: u64,
}

impl Hasher for FastHasher {
    fn write(&mut self, bytes: &[u8]) {
        self.state = fold_mul(self.state ^ hash_bytes(bytes), K1);
    }

    fn write_u64(&mut self, x: u64) {
        self.state = fold_mul(self.state ^ x ^ K0, K1);
    }

    fn write_u32(&mut self, x: u32) {
        self.write_u64(x as u64);
    }

    fn write_u16(&mut self, x: u16) {
        self.write_u64(x as u64);
    }

    fn write_u8(&mut self, x: u8) {
        self.write_u64(x as u64);
    }

    fn write_usize(&mut self, x: usize) {
        self.write_u64(x as u64);
    }

    fn write_u128(&mut self, x: u128) {
        self.write_u64(x as u64);
        self.write_u64((x >> 64) as u64);
    }

    fn finish(&self) -> u64 {
        fold_mul(self.state, K2)
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FastBuildHasher;

impl BuildHasher for FastBuildHasher {
    type Hasher = FastHasher;

    fn build_hasher(&self) -> FastHasher {
        FastHasher::default()
    }
}

pub type FastHashMap<K, V> = std::collections::HashMap<K, V, FastBuildHasher>;
pub type FastHashSet<K> = std::collections::HashSet<K, FastBuildHasher>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn xxh64_known_vectors() {
        // Expected values computed with the reference libxxhash (XXH64).
        assert_eq!(xxh64(b"", 0), 0xEF46_DB37_51D8_E999);
        assert_eq!(xxh64(b"a", 0), 0xD24E_C4F1_A98C_6E5B);
        assert_eq!(xxh64(b"abc", 0), 0x44BC_2CF5_AD77_0999);
        assert_eq!(
            xxh64(b"Nobody inspects the spammish repetition", 0),
            0xFBCE_A83C_8A37_8BF1
        );
        assert_eq!(xxh64(b"xxhash", 0), 0x32DD_3895_2C4B_C720);
        assert_eq!(xxh64(b"xxhash", 20), 0x48B3_5AA9_8DC0_4F56);
    }

    #[test]
    fn xxh64_covers_every_tail_length() {
        let data: Vec<u8> = (0..100u8).collect();
        let hashes: HashSet<u64> = (0..=data.len()).map(|n| xxh64(&data[..n], 0)).collect();
        assert_eq!(hashes.len(), data.len() + 1);
    }

    #[test]
    fn canonical_hash_of_values() {
        assert_eq!(xxh64_value(&Value::Null), None);
        assert_eq!(
            xxh64_value(&Value::Int64(42)),
            Some(xxh64(&42i64.to_le_bytes(), 0))
        );
        // Every integer width hashes as i64.
        assert_eq!(
            xxh64_value(&Value::Int16(42)),
            xxh64_value(&Value::Int64(42))
        );
        assert_eq!(
            xxh64_value(&Value::Int32(-1)),
            xxh64_value(&Value::Int64(-1))
        );
        assert_eq!(
            xxh64_value(&Value::Int128(42)),
            Some(xxh64(&42i128.to_le_bytes(), 0))
        );
        assert_eq!(xxh64_value(&Value::varchar("abc")), Some(xxh64(b"abc", 0)));
        assert_eq!(
            xxh64_value(&Value::Float64(-0.0)),
            xxh64_value(&Value::Float64(0.0))
        );
        let wide = Value::Decimal {
            value: 7,
            precision: 20,
            scale: 2,
        };
        assert_eq!(xxh64_value(&wide), Some(xxh64(&7i128.to_le_bytes(), 0)));
    }

    #[test]
    fn fast_hashes_spread() {
        let ints: HashSet<u64> = (0..10_000u64).map(hash_u64).collect();
        assert_eq!(ints.len(), 10_000);
        let wide: HashSet<u64> = (0..10_000u128).map(|x| hash_u128(x << 64)).collect();
        assert_eq!(wide.len(), 10_000);
        let strings: HashSet<u64> = (0..10_000)
            .map(|i| hash_bytes(format!("key-{i}").as_bytes()))
            .collect();
        assert_eq!(strings.len(), 10_000);
        assert_ne!(hash_bytes(b"a"), hash_bytes(b"a\0"));
        assert_ne!(hash_bytes(b""), hash_bytes(b"\0"));
        // Low bits are used for bucket selection: they must vary too.
        let buckets: HashSet<u64> = (0..1024u64).map(|x| hash_u64(x) & 1023).collect();
        assert!(buckets.len() > 500);
    }

    #[test]
    fn hash_map_with_fast_hasher() {
        let mut map: FastHashMap<Vec<u8>, usize> = FastHashMap::default();
        for i in 0..1000usize {
            map.insert(i.to_string().into_bytes(), i);
        }
        assert_eq!(map.len(), 1000);
        assert_eq!(map[&b"512".to_vec()], 512);
        let a = FastBuildHasher.hash_one(12345u64);
        let b = FastBuildHasher.hash_one(12345u64);
        assert_eq!(a, b);
    }
}
