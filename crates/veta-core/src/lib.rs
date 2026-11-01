//! Core types shared by every veta crate: data types, values, vectors, errors, hashing,
//! CRC, HyperLogLog, PRNG, dates, decimals, TSV output, statistics, scan filters and the
//! memory tracker (contract §2, §4, §12, §13).
//!
//! `unsafe` is denied crate-wide; the only exception is the SSE4.2 CRC32C dispatch in
//! [`crc`], which the contract places in this crate (§2, §12).

#![deny(unsafe_code)]

pub mod bitmap;
#[allow(unsafe_code)]
pub mod crc;
pub mod datetime;
pub mod decimal;
pub mod error;
pub mod format;
pub mod hash;
pub mod hll;
pub mod memory;
pub mod predicate;
pub mod rand;
pub mod stats;
pub mod types;
pub mod value;
pub mod vector;

pub use bitmap::Bitmap;
pub use error::{Result, VetaError};
pub use types::{DataType, Field, Schema};
pub use value::Value;
pub use vector::{Chunk, Vector, VectorData};
