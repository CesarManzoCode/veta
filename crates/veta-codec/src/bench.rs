//! In-memory compressor benchmark behind `veta codec-bench` (contract §13, §16).
//!
//! One thread, best of at least `iterations` (minimum 3) runs per direction, and at least
//! [`MIN_TIME`] of measurement in total, half compressing and half decompressing. Speeds
//! are in MB/s of raw (uncompressed) bytes, with 1 MB = 10⁶ bytes.

use std::hint::black_box;
use std::time::{Duration, Instant};

use veta_core::{Result, VetaError};

use crate::{snappy, vlz};

/// Minimum total measurement time of [`bench_compressor`].
pub const MIN_TIME: Duration = Duration::from_secs(1);

/// Minimum number of runs per direction.
pub const MIN_ITERATIONS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compressor {
    Vlz,
    Snappy,
}

impl Compressor {
    /// Parses the `--compressor` name of `codec-bench`.
    pub fn from_name(name: &str) -> Result<Compressor> {
        match name {
            "vlz" => Ok(Compressor::Vlz),
            "snappy" => Ok(Compressor::Snappy),
            other => Err(VetaError::user(format!("unknown compressor '{other}'"))),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Compressor::Vlz => "vlz",
            Compressor::Snappy => "snappy",
        }
    }

    pub fn compress(self, data: &[u8]) -> Vec<u8> {
        match self {
            Compressor::Vlz => vlz::compress(data),
            Compressor::Snappy => snappy::compress(data),
        }
    }

    pub fn decompress(self, data: &[u8]) -> Result<Vec<u8>> {
        match self {
            Compressor::Vlz => vlz::decompress(data),
            Compressor::Snappy => snappy::decompress(data),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BenchResult {
    pub raw_bytes: u64,
    pub compressed_bytes: u64,
    pub compress_mb_s: f64,
    pub decompress_mb_s: f64,
}

/// Benchmarks `compressor` on `data` (contract §13 `codec-bench`). Fails if the round
/// trip does not reproduce `data`.
pub fn bench_compressor(
    compressor: Compressor,
    data: &[u8],
    iterations: usize,
) -> Result<BenchResult> {
    bench_for(compressor, data, iterations, MIN_TIME)
}

fn mb_s(bytes: usize, best: Duration) -> f64 {
    if bytes == 0 {
        return 0.0;
    }
    bytes as f64 / 1e6 / best.as_secs_f64().max(1e-9)
}

/// Runs `f` at least `iterations` times and for at least `min_time`; best duration.
fn best_of(iterations: usize, min_time: Duration, mut f: impl FnMut()) -> Duration {
    let start = Instant::now();
    let mut best = Duration::MAX;
    let mut runs = 0;
    while runs < iterations || start.elapsed() < min_time {
        let t = Instant::now();
        f();
        best = best.min(t.elapsed());
        runs += 1;
    }
    best
}

fn bench_for(
    compressor: Compressor,
    data: &[u8],
    iterations: usize,
    min_time: Duration,
) -> Result<BenchResult> {
    let iterations = iterations.max(MIN_ITERATIONS);
    let compressed = compressor.compress(data);
    if compressor.decompress(&compressed)? != data {
        return Err(VetaError::internal(format!(
            "{} round trip does not reproduce the input",
            compressor.name()
        )));
    }
    let half = min_time / 2;
    let compress = best_of(iterations, half, || {
        black_box(compressor.compress(black_box(data)));
    });
    let mut failed = false;
    let decompress = best_of(iterations, half, || {
        failed |= black_box(compressor.decompress(black_box(&compressed))).is_err();
    });
    if failed {
        return Err(VetaError::internal(format!(
            "{} decompression failed during the benchmark",
            compressor.name()
        )));
    }
    Ok(BenchResult {
        raw_bytes: data.len() as u64,
        compressed_bytes: compressed.len() as u64,
        compress_mb_s: mb_s(data.len(), compress),
        decompress_mb_s: mb_s(data.len(), decompress),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(Compressor::from_name("vlz").unwrap(), Compressor::Vlz);
        assert_eq!(Compressor::from_name("snappy").unwrap(), Compressor::Snappy);
        assert_eq!(Compressor::from_name("lz4").unwrap_err().exit_code(), 1);
        assert_eq!(Compressor::Vlz.name(), "vlz");
    }

    #[test]
    fn bench_reports_sizes_and_speeds() {
        let data = "veta,clickhouse,duckdb\n".repeat(4000).into_bytes();
        for compressor in [Compressor::Vlz, Compressor::Snappy] {
            let r = bench_for(compressor, &data, 1, Duration::from_millis(20)).unwrap();
            assert_eq!(r.raw_bytes, data.len() as u64);
            assert_eq!(r.compressed_bytes, compressor.compress(&data).len() as u64);
            assert!(r.compressed_bytes < r.raw_bytes);
            assert!(r.compress_mb_s > 0.0 && r.compress_mb_s.is_finite());
            assert!(r.decompress_mb_s > 0.0 && r.decompress_mb_s.is_finite());
        }
    }

    #[test]
    fn best_of_runs_at_least_the_minimum() {
        let mut runs = 0;
        best_of(MIN_ITERATIONS, Duration::ZERO, || runs += 1);
        assert_eq!(runs, MIN_ITERATIONS);
        let start = Instant::now();
        best_of(1, Duration::from_millis(30), || {});
        assert!(start.elapsed() >= Duration::from_millis(30));
    }

    #[test]
    fn empty_input() {
        let r = bench_for(Compressor::Vlz, &[], 3, Duration::ZERO).unwrap();
        assert_eq!(r.raw_bytes, 0);
        assert_eq!(r.compress_mb_s, 0.0);
    }
}
