//! Engine errors and their CLI exit codes (contract §13).

use std::fmt;

pub type Result<T> = std::result::Result<T, VetaError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VetaError {
    /// Syntax, binding, `unsupported: …`, overflow and other errors caused by the input.
    User(String),
    /// Filesystem or other I/O failure.
    Io(String),
    /// Broken engine invariant.
    Internal(String),
    /// A part failed its integrity checks (contract §6.4).
    Corrupt { part: String, reason: String },
    /// The global memory tracker refused a reservation (contract §12).
    MemoryLimit { requested: u64, limit: u64 },
}

impl VetaError {
    pub fn user(msg: impl Into<String>) -> Self {
        VetaError::User(msg.into())
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        VetaError::Internal(msg.into())
    }

    /// `unsupported: <what>`, exit 1 (contract §10.3).
    pub fn unsupported(what: impl fmt::Display) -> Self {
        VetaError::User(format!("unsupported: {what}"))
    }

    pub fn corrupt(part: impl Into<String>, reason: impl Into<String>) -> Self {
        VetaError::Corrupt {
            part: part.into(),
            reason: reason.into(),
        }
    }

    /// Exit code of the CLI (contract §13): 1 user, 2 I/O or internal, 3 corrupt, 4 memory.
    pub fn exit_code(&self) -> u8 {
        match self {
            VetaError::User(_) => 1,
            VetaError::Io(_) | VetaError::Internal(_) => 2,
            VetaError::Corrupt { .. } => 3,
            VetaError::MemoryLimit { .. } => 4,
        }
    }
}

impl fmt::Display for VetaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VetaError::User(msg) => f.write_str(msg),
            VetaError::Io(msg) => write!(f, "io error: {msg}"),
            VetaError::Internal(msg) => write!(f, "internal error: {msg}"),
            VetaError::Corrupt { part, reason } => write!(f, "corrupt part {part}: {reason}"),
            VetaError::MemoryLimit { requested, limit } => write!(
                f,
                "memory limit exceeded: requested {requested} bytes, limit {limit} bytes"
            ),
        }
    }
}

impl std::error::Error for VetaError {}

impl From<std::io::Error> for VetaError {
    fn from(err: std::io::Error) -> Self {
        VetaError::Io(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_follow_the_cli_contract() {
        assert_eq!(VetaError::user("x").exit_code(), 1);
        assert_eq!(VetaError::unsupported("UNION").exit_code(), 1);
        assert_eq!(VetaError::Io("x".into()).exit_code(), 2);
        assert_eq!(VetaError::internal("x").exit_code(), 2);
        assert_eq!(VetaError::corrupt("all_1_1_0", "crc").exit_code(), 3);
        let mem = VetaError::MemoryLimit {
            requested: 10,
            limit: 5,
        };
        assert_eq!(mem.exit_code(), 4);
    }

    #[test]
    fn messages() {
        assert_eq!(
            VetaError::unsupported("UNION").to_string(),
            "unsupported: UNION"
        );
        assert_eq!(
            VetaError::corrupt("all_1_1_0", "bad crc").to_string(),
            "corrupt part all_1_1_0: bad crc"
        );
    }

    #[test]
    fn io_errors_convert() {
        let io = std::io::Error::new(std::io::ErrorKind::NotFound, "gone");
        let err: VetaError = io.into();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("gone"));
    }
}
