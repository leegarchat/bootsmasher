//! bootsmasher — shared error type (hand-rolled, no external crates).

use std::fmt;

/// All errors surface as a message on stderr with a non-zero exit code.
/// Nothing binary is ever written to stdout on error.
#[derive(Debug)]
pub enum Error {
    Usage(String),
    /// Operational failure with exit code 1 but no help dump
    /// (cpio `exists` on a missing entry, like magiskboot).
    /// Unused in the `small` build (no cpio subprogram there).
    #[cfg_attr(feature = "small", allow(dead_code))]
    Fail(String),
    Io(String),
    Parse(String),
    Verify(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Usage(m) | Error::Fail(m) | Error::Io(m) | Error::Parse(m) | Error::Verify(m) => {
                write!(f, "{m}")
            }
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(format!("io error: {e}"))
    }
}

pub type Result<T> = std::result::Result<T, Error>;
