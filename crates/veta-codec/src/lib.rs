//! Column encodings, the `vlz` general-purpose compressor and Snappy (contract §8).
//!
//! Depends only on `veta-core` (contract §2). The column encodings of §8.1 live in
//! [`encoding`] on top of [`bitpack`]; `vlz` and Snappy arrive in later sprints.

pub mod bitpack;
pub mod encoding;

pub use encoding::{Encoding, decode, encode, encode_with};
