//! Compact Thrift, Parquet metadata, reader and writer (contract §9).
//!
//! Depends on `veta-core` and `veta-codec` (contract §2). [`thrift`] implements the compact
//! protocol, [`meta`] the metadata structs of `parquet.thrift` and [`footer`] reading and
//! writing the file footer. The page reader and writer arrive in later sprints.

#![deny(unsafe_code)]

pub mod footer;
pub mod meta;
pub mod thrift;

pub use footer::{read_footer, write_footer};
pub use meta::{FileMetaData, PageHeader, ThriftStruct};

use veta_core::VetaError;

/// Error for a malformed Parquet file: `invalid parquet: <what>` (exit 1, a bad input).
pub(crate) fn invalid(what: impl std::fmt::Display) -> VetaError {
    VetaError::user(format!("invalid parquet: {what}"))
}
