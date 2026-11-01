//! Deterministic PRNG: xoshiro256** seeded through SplitMix64. Same seed, same sequence on
//! every platform.

/// One SplitMix64 step: advances `state` and returns the next output.
pub fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rng {
    s: [u64; 4],
}

impl Rng {
    pub fn new(seed: u64) -> Rng {
        let mut sm = seed;
        Rng {
            s: [
                splitmix64(&mut sm),
                splitmix64(&mut sm),
                splitmix64(&mut sm),
                splitmix64(&mut sm),
            ],
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        let s = &mut self.s;
        let result = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        result
    }

    pub fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    /// Uniform in `[0, 1)` with 53 random bits.
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// `true` with probability `p`.
    pub fn next_bool(&mut self, p: f64) -> bool {
        self.next_f64() < p
    }

    /// Uniform in `[0, n)` without modulo bias (Lemire). Panics if `n == 0`.
    pub fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "below(0)");
        let mut m = (self.next_u64() as u128) * (n as u128);
        if (m as u64) < n {
            let threshold = n.wrapping_neg() % n;
            while (m as u64) < threshold {
                m = (self.next_u64() as u128) * (n as u128);
            }
        }
        (m >> 64) as u64
    }

    /// Uniform in `[lo, hi]` (inclusive). Panics if `lo > hi`.
    pub fn range_i64(&mut self, lo: i64, hi: i64) -> i64 {
        assert!(lo <= hi, "empty range");
        let span = hi as i128 - lo as i128 + 1;
        if span > u64::MAX as i128 {
            return self.next_u64() as i64;
        }
        (lo as i128 + self.below(span as u64) as i128) as i64
    }

    pub fn fill_bytes(&mut self, out: &mut [u8]) {
        for chunk in out.chunks_mut(8) {
            let bytes = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }

    /// Fisher–Yates shuffle.
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            items.swap(i, j);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splitmix64_reference_outputs() {
        let mut state = 0;
        assert_eq!(splitmix64(&mut state), 0xE220_A839_7B1D_CDAF);
        assert_eq!(splitmix64(&mut state), 0x6E78_9E6A_A1B9_65F4);
        assert_eq!(splitmix64(&mut state), 0x06C4_5D18_8009_454F);
    }

    #[test]
    fn deterministic_per_seed() {
        let a: Vec<u64> = {
            let mut rng = Rng::new(42);
            (0..100).map(|_| rng.next_u64()).collect()
        };
        let b: Vec<u64> = {
            let mut rng = Rng::new(42);
            (0..100).map(|_| rng.next_u64()).collect()
        };
        let c: Vec<u64> = {
            let mut rng = Rng::new(43);
            (0..100).map(|_| rng.next_u64()).collect()
        };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn ranges_stay_in_bounds() {
        let mut rng = Rng::new(1);
        let mut seen = [false; 10];
        for _ in 0..10_000 {
            let x = rng.below(10);
            assert!(x < 10);
            seen[x as usize] = true;
            let y = rng.range_i64(-5, 5);
            assert!((-5..=5).contains(&y));
            let f = rng.next_f64();
            assert!((0.0..1.0).contains(&f));
        }
        assert!(seen.iter().all(|&s| s));
        assert_eq!(rng.range_i64(7, 7), 7);
        rng.range_i64(i64::MIN, i64::MAX);
    }

    #[test]
    fn below_is_roughly_uniform() {
        let mut rng = Rng::new(99);
        let mut counts = [0u32; 4];
        for _ in 0..40_000 {
            counts[rng.below(4) as usize] += 1;
        }
        assert!(
            counts.iter().all(|&c| (9_000..11_000).contains(&c)),
            "{counts:?}"
        );
    }

    #[test]
    fn shuffle_is_a_permutation() {
        let mut rng = Rng::new(5);
        let mut items: Vec<u32> = (0..100).collect();
        rng.shuffle(&mut items);
        assert_ne!(items, (0..100).collect::<Vec<_>>());
        items.sort();
        assert_eq!(items, (0..100).collect::<Vec<_>>());
        let mut bytes = [0u8; 13];
        rng.fill_bytes(&mut bytes);
        assert!(bytes.iter().any(|&b| b != 0));
    }
}
