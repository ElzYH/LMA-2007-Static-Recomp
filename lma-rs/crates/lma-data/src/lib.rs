//! Readers and writers for LMA Manager 2007 data files: the Xbox 360's, and the PS2's database
//! (same layout, little-endian, #3).
//!
//! Nothing from the game is bundled: everything is read from the user's own copy.

pub mod archive;
pub mod bits;
pub mod csv;
pub mod db;
pub mod diary;
pub mod disc;
pub mod edit;
pub mod font;
pub mod lang;
pub mod league;
#[cfg(any(test, feature = "test-fixtures"))]
#[doc(hidden)]
pub mod fixtures;
pub mod text;
pub mod textures;
pub mod xact;

pub use archive::Archive;
pub use db::{Database, Player, Position};

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Format(String),
    Value(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Format(m) => write!(f, "bad file: {m}"),
            Error::Value(m) => write!(f, "{m}"),
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

/// Byte order of a data file: the Xbox 360's `BG2` archives are big-endian, the PS2's `MG2`
/// archives little-endian (#3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Endian {
    Big,
    Little,
}

impl Endian {
    pub(crate) fn u32(self, d: &[u8], off: usize) -> u32 {
        let b = d[off..off + 4].try_into().unwrap();
        match self {
            Endian::Big => u32::from_be_bytes(b),
            Endian::Little => u32::from_le_bytes(b),
        }
    }

    pub(crate) fn u16(self, d: &[u8], off: usize) -> u16 {
        let b = d[off..off + 2].try_into().unwrap();
        match self {
            Endian::Big => u16::from_be_bytes(b),
            Endian::Little => u16::from_le_bytes(b),
        }
    }

    pub(crate) fn u32_bytes(self, v: u32) -> [u8; 4] {
        match self {
            Endian::Big => v.to_be_bytes(),
            Endian::Little => v.to_le_bytes(),
        }
    }

    pub(crate) fn u16_bytes(self, v: u16) -> [u8; 2] {
        match self {
            Endian::Big => v.to_be_bytes(),
            Endian::Little => v.to_le_bytes(),
        }
    }
}
