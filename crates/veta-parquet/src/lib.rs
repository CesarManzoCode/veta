//! Compact Thrift, Parquet metadata, reader and writer (contract §9).
//!
//! Depends on `veta-core` and `veta-codec` (contract §2). [`thrift`] implements the compact
//! protocol, [`meta`] the metadata structs of `parquet.thrift` and [`footer`] reading and
//! writing the file footer, [`encodings`] the value and level decoders, [`reader`] the
//! flat-schema reader with row-group statistics, [`encoders`] the value and level encoders
//! and [`writer`] the streaming writer of `veta export`.

#![deny(unsafe_code)]

pub mod encoders;
pub mod encodings;
pub mod footer;
pub mod meta;
pub mod reader;
pub mod thrift;
pub mod writer;

pub use footer::{read_footer, write_footer};
pub use meta::{FileMetaData, PageHeader, ThriftStruct};
pub use reader::{CHUNK_ROWS, ChunkIter, ParquetFile};
pub use writer::{Compression, PageVersion, ParquetWriter, ValueEncoding, WriterOptions};

use veta_core::VetaError;

/// Error for a malformed Parquet file: `invalid parquet: <what>` (exit 1, a bad input).
pub(crate) fn invalid(what: impl std::fmt::Display) -> VetaError {
    VetaError::user(format!("invalid parquet: {what}"))
}
