//! Parquet file footer (contract §9): the file starts with `PAR1` and ends with the
//! compact-Thrift `FileMetaData`, its length as a little-endian `u32` and `PAR1` again.
//! A file whose trailing magic is `PARE` uses an encrypted footer and is rejected.

use crate::invalid;
use crate::meta::{FileMetaData, decode, encode};
use std::fs::File;
use std::io::Write;
use std::os::unix::fs::FileExt;
use veta_core::{Result, VetaError};

pub const MAGIC: &[u8; 4] = b"PAR1";
const ENCRYPTED_MAGIC: &[u8; 4] = b"PARE";

/// Leading magic, trailing length and trailing magic.
pub const MIN_FILE_LEN: u64 = 12;

/// Validates the last 8 bytes of a file of `file_len` bytes and returns the metadata
/// length.
pub fn parse_tail(tail: &[u8; 8], file_len: u64) -> Result<u32> {
    if &tail[4..] == ENCRYPTED_MAGIC {
        return Err(VetaError::unsupported("encrypted parquet footer"));
    }
    if &tail[4..] != MAGIC {
        return Err(invalid("missing PAR1 magic at end of file"));
    }
    let len = u32::from_le_bytes(tail[..4].try_into().expect("4 bytes"));
    if u64::from(len) > file_len.saturating_sub(MIN_FILE_LEN) {
        return Err(invalid(format!(
            "footer length {len} does not fit in a file of {file_len} bytes"
        )));
    }
    Ok(len)
}

/// Reads and decodes the footer of `file`.
pub fn read_footer(file: &File) -> Result<FileMetaData> {
    let file_len = file.metadata()?.len();
    if file_len < MIN_FILE_LEN {
        return Err(invalid(format!("file of {file_len} bytes is too short")));
    }
    let mut head = [0u8; 4];
    file.read_exact_at(&mut head, 0)?;
    if &head != MAGIC {
        return Err(invalid("missing PAR1 magic at start of file"));
    }
    let mut tail = [0u8; 8];
    file.read_exact_at(&mut tail, file_len - 8)?;
    let len = parse_tail(&tail, file_len)?;
    let mut buf = vec![0u8; len as usize];
    file.read_exact_at(&mut buf, file_len - 8 - u64::from(len))?;
    let (meta, used) = decode::<FileMetaData>(&buf)?;
    if used != buf.len() {
        return Err(invalid(format!(
            "footer declares {len} bytes but its metadata takes {used}"
        )));
    }
    Ok(meta)
}

/// The bytes that end a file: encoded `meta`, its length and `PAR1`.
pub fn encode_footer(meta: &FileMetaData) -> Result<Vec<u8>> {
    let mut out = encode(meta);
    let len = u32::try_from(out.len())
        .map_err(|_| VetaError::internal("parquet footer longer than u32::MAX bytes"))?;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(MAGIC);
    Ok(out)
}

/// Writes the footer of `meta` to `w` (after the column chunks); returns the bytes written.
pub fn write_footer<W: Write>(w: &mut W, meta: &FileMetaData) -> Result<u64> {
    let bytes = encode_footer(meta)?;
    w.write_all(&bytes)?;
    Ok(bytes.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::{FieldRepetitionType, SchemaElement, Type};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct TempFile(PathBuf);

    impl TempFile {
        fn with(bytes: &[u8]) -> TempFile {
            static N: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "veta-footer-{}-{}.parquet",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::write(&path, bytes).unwrap();
            TempFile(path)
        }

        fn read(&self) -> Result<FileMetaData> {
            read_footer(&File::open(&self.0).unwrap())
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn sample() -> FileMetaData {
        FileMetaData {
            version: 1,
            schema: vec![
                SchemaElement {
                    type_: None,
                    type_length: None,
                    repetition_type: None,
                    name: "schema".into(),
                    num_children: Some(1),
                    converted_type: None,
                    scale: None,
                    precision: None,
                    field_id: None,
                    logical_type: None,
                },
                SchemaElement {
                    type_: Some(Type::Int32),
                    type_length: None,
                    repetition_type: Some(FieldRepetitionType::Required),
                    name: "a".into(),
                    num_children: None,
                    converted_type: None,
                    scale: None,
                    precision: None,
                    field_id: None,
                    logical_type: None,
                },
            ],
            num_rows: 0,
            row_groups: Vec::new(),
            key_value_metadata: None,
            created_by: Some("veta 0.1.0".into()),
            column_orders: None,
        }
    }

    fn file_bytes(meta: &FileMetaData) -> Vec<u8> {
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&[1, 2, 3]); // stand-in column data
        write_footer(&mut bytes, meta).unwrap();
        bytes
    }

    #[test]
    fn footer_round_trip() {
        let meta = sample();
        let bytes = file_bytes(&meta);
        assert_eq!(&bytes[bytes.len() - 4..], MAGIC);
        let f = TempFile::with(&bytes);
        assert_eq!(f.read().unwrap(), meta);
    }

    #[test]
    fn wrong_trailing_magic_fails() {
        let mut bytes = file_bytes(&sample());
        let n = bytes.len();
        bytes[n - 1] = b'X';
        assert!(TempFile::with(&bytes).read().is_err());
    }

    #[test]
    fn wrong_leading_magic_fails() {
        let mut bytes = file_bytes(&sample());
        bytes[0] = b'Q';
        assert!(TempFile::with(&bytes).read().is_err());
    }

    #[test]
    fn encrypted_footer_is_unsupported() {
        let mut bytes = file_bytes(&sample());
        let n = bytes.len();
        bytes[n - 4..].copy_from_slice(b"PARE");
        let err = TempFile::with(&bytes).read().unwrap_err();
        assert!(err.to_string().contains("encrypted"), "{err}");
    }

    #[test]
    fn short_file_and_oversized_length_fail() {
        assert!(TempFile::with(b"PAR1PAR1").read().is_err());
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&1000u32.to_le_bytes());
        bytes.extend_from_slice(MAGIC);
        assert!(TempFile::with(&bytes).read().is_err());
    }

    #[test]
    fn length_that_does_not_match_metadata_fails() {
        let meta = sample();
        let encoded = encode(&meta);
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(&encoded);
        bytes.push(0); // a stray byte counted in the length
        bytes.extend_from_slice(&(encoded.len() as u32 + 1).to_le_bytes());
        bytes.extend_from_slice(MAGIC);
        assert!(TempFile::with(&bytes).read().is_err());
    }

    #[test]
    fn parse_tail_checks_bounds() {
        let mut tail = [0u8; 8];
        tail[..4].copy_from_slice(&8u32.to_le_bytes());
        tail[4..].copy_from_slice(MAGIC);
        assert_eq!(parse_tail(&tail, 20).unwrap(), 8);
        assert!(parse_tail(&tail, 19).is_err());
    }
}
