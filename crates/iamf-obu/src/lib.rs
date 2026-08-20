//! Bitstream model and parser for IAMF (Immersive Audio Model and Formats).
//!
//! This crate covers the security-critical front half of an IAMF decoder: OBU
//! framing and descriptor parsing of untrusted input. It is deliberately free
//! of DSP; reconstruction, rendering, and mixing live in `iamf-dec`.
//!
//! Spec: <https://aomediacodec.github.io/iamf/>

#![forbid(unsafe_code)]

mod error;
mod leb128;
mod obu;
mod reader;

pub mod descriptors;

pub use error::Error;
pub use obu::{Obu, ObuHeader, ObuIter, ObuType};
pub use reader::ByteReader;

pub type Result<T> = core::result::Result<T, Error>;
