//! Column encodings, the `vlz` general-purpose compressor and Snappy (contract §8).
//!
//! Depends only on `veta-core` (contract §2). The column encodings of §8.1 live in
//! [`encoding`] on top of [`bitpack`]; the `vlz` compressor (§8.2) in [`vlz`], raw Snappy
//! (§8.3) in [`snappy`] and the `codec-bench` measurement (§13) in [`bench`]. `unsafe` is
//! used only in the hot loop of the `vlz` decompressor, each block with its `// SAFETY:`.

pub mod bench;
pub mod bitpack;
pub mod encoding;
pub mod snappy;
pub mod vlz;

pub use bench::{BenchResult, Compressor, bench_compressor};
pub use encoding::{DictCodes, Encoding, decode, decode_keeping_codes, encode, encode_with};
