//! HyperLogLog with p = 12: 4096 `u8` registers fed by xxh64 (seed 0) (contract §5.2, §11.5).
//!
//! The register index is the top 12 bits of the hash and the rank is one plus the number of
//! leading zeros of the remaining 52 bits (at most 53).

use crate::error::{Result, VetaError};
use crate::hash::{xxh64, xxh64_value};
use crate::value::Value;

pub const HLL_P: u32 = 12;
pub const HLL_REGISTERS: usize = 1 << HLL_P;
const MAX_RANK: u8 = (64 - HLL_P + 1) as u8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hll {
    registers: Vec<u8>,
}

impl Default for Hll {
    fn default() -> Self {
        Hll::new()
    }
}

impl Hll {
    pub fn new() -> Self {
        Hll {
            registers: vec![0; HLL_REGISTERS],
        }
    }

    /// Adds a 64-bit hash.
    pub fn add_hash(&mut self, hash: u64) {
        let index = (hash >> (64 - HLL_P)) as usize;
        let rest = hash << HLL_P;
        let rank = (rest.leading_zeros().min(64 - HLL_P) + 1) as u8;
        if rank > self.registers[index] {
            self.registers[index] = rank;
        }
    }

    /// Adds a byte string (hashed with xxh64, seed 0).
    pub fn add(&mut self, bytes: &[u8]) {
        self.add_hash(xxh64(bytes, 0));
    }

    /// Adds a value by its canonical hash (contract §12); NULL is ignored.
    pub fn add_value(&mut self, value: &Value) {
        if let Some(hash) = xxh64_value(value) {
            self.add_hash(hash);
        }
    }

    /// Union: the register-wise maximum.
    pub fn merge(&mut self, other: &Hll) {
        for (a, &b) in self.registers.iter_mut().zip(&other.registers) {
            if b > *a {
                *a = b;
            }
        }
    }

    /// Cardinality estimate with the small-range (linear counting) correction.
    pub fn estimate(&self) -> f64 {
        let m = HLL_REGISTERS as f64;
        let alpha = 0.7213 / (1.0 + 1.079 / m);
        let mut sum = 0.0;
        let mut zeros = 0usize;
        for &r in &self.registers {
            sum += 2f64.powi(-(r as i32));
            if r == 0 {
                zeros += 1;
            }
        }
        let raw = alpha * m * m / sum;
        if raw <= 2.5 * m && zeros > 0 {
            m * (m / zeros as f64).ln()
        } else {
            raw
        }
    }

    pub fn registers(&self) -> &[u8] {
        &self.registers
    }

    /// The 4096 registers, one byte each.
    pub fn to_bytes(&self) -> Vec<u8> {
        self.registers.clone()
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Hll> {
        if bytes.len() != HLL_REGISTERS {
            return Err(VetaError::internal(format!(
                "HLL has {} bytes, expected {HLL_REGISTERS}",
                bytes.len()
            )));
        }
        if let Some(bad) = bytes.iter().find(|&&r| r > MAX_RANK) {
            return Err(VetaError::internal(format!(
                "HLL register {bad} exceeds {MAX_RANK}"
            )));
        }
        Ok(Hll {
            registers: bytes.to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn relative_error(estimate: f64, truth: f64) -> f64 {
        (estimate - truth).abs() / truth
    }

    #[test]
    fn empty_estimates_zero() {
        assert_eq!(Hll::new().estimate(), 0.0);
    }

    #[test]
    fn error_under_five_percent_for_100k_distinct() {
        let mut hll = Hll::new();
        for i in 0..100_000i64 {
            hll.add_value(&Value::Int64(i));
            hll.add_value(&Value::Int64(i));
        }
        let err = relative_error(hll.estimate(), 100_000.0);
        assert!(err < 0.05, "error {err}");

        let mut strings = Hll::new();
        for i in 0..100_000 {
            strings.add(format!("https://example.com/{i}").as_bytes());
        }
        let err = relative_error(strings.estimate(), 100_000.0);
        assert!(err < 0.05, "error {err}");
    }

    #[test]
    fn small_range_is_close() {
        let mut hll = Hll::new();
        for i in 0..100i64 {
            hll.add_value(&Value::Int64(i));
        }
        assert!(relative_error(hll.estimate(), 100.0) < 0.05);
        hll.add_value(&Value::Null);
        assert!(relative_error(hll.estimate(), 100.0) < 0.05);
    }

    #[test]
    fn merge_equals_union() {
        let mut a = Hll::new();
        let mut b = Hll::new();
        let mut all = Hll::new();
        for i in 0..60_000i64 {
            a.add_value(&Value::Int64(i));
        }
        for i in 40_000..100_000i64 {
            b.add_value(&Value::Int64(i));
        }
        for i in 0..100_000i64 {
            all.add_value(&Value::Int64(i));
        }
        a.merge(&b);
        assert_eq!(a, all);
        assert_eq!(a.estimate(), all.estimate());
    }

    #[test]
    fn rank_bounds() {
        let mut hll = Hll::new();
        hll.add_hash(0);
        assert_eq!(hll.registers()[0], 53);
        hll.add_hash(u64::MAX);
        assert_eq!(hll.registers()[HLL_REGISTERS - 1], 1);
    }

    #[test]
    fn serialization_round_trips() {
        let mut hll = Hll::new();
        for i in 0..5000i64 {
            hll.add_value(&Value::Int64(i));
        }
        let bytes = hll.to_bytes();
        assert_eq!(bytes.len(), 4096);
        assert_eq!(Hll::from_bytes(&bytes).unwrap(), hll);
        assert!(Hll::from_bytes(&bytes[1..]).is_err());
        let mut bad = bytes.clone();
        bad[3] = 54;
        assert!(Hll::from_bytes(&bad).is_err());
    }
}
