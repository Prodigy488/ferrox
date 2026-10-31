//! Readers for the original Xbox's file formats: XDVDFS disc images (XISO)
//! and XBE executables. No dependencies, no unsafe, and every field is bounds
//! checked so a damaged or hostile file produces an error, never a panic.

pub mod xbe;
pub mod xdvdfs;

use std::fmt;

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// Not a recognised file of this kind.
    Format(&'static str),
    /// Recognised, but internally inconsistent.
    Corrupt(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "I/O error: {e}"),
            Error::Format(m) => write!(f, "{m}"),
            Error::Corrupt(m) => write!(f, "damaged file: {m}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn le16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

pub(crate) fn le32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}
