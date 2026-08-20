use core::fmt;

/// Parse errors. All variants are recoverable; the parser never panics on
/// malformed input (enforced by the fuzz targets in `fuzz/`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// Input ended before a complete syntax element was read.
    UnexpectedEof {
        /// Byte offset at which more data was needed.
        offset: usize,
    },
    /// A leb128 value was longer than the 8-byte maximum or overflowed u32.
    InvalidLeb128 { offset: usize },
    /// An OBU declared a size that is inconsistent with its own fields
    /// (e.g. smaller than its trimming/extension headers).
    InvalidObuSize { offset: usize },
    /// A reserved OBU type (25..=30) was encountered.
    ReservedObuType { obu_type: u8, offset: usize },
    /// A descriptor payload violated the spec (bad 4CC, zero count where
    /// nonzero is required, reserved enum value, ...).
    InvalidDescriptor { offset: usize },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::UnexpectedEof { offset } => {
                write!(f, "unexpected end of input at byte {offset}")
            }
            Error::InvalidLeb128 { offset } => {
                write!(f, "invalid leb128 at byte {offset}")
            }
            Error::InvalidObuSize { offset } => {
                write!(f, "inconsistent obu_size at byte {offset}")
            }
            Error::ReservedObuType { obu_type, offset } => {
                write!(f, "reserved OBU type {obu_type} at byte {offset}")
            }
            Error::InvalidDescriptor { offset } => {
                write!(f, "invalid descriptor near byte {offset}")
            }
        }
    }
}

impl std::error::Error for Error {}
