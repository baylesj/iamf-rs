//! [`SubstreamDecoder`] implementations for the codecs IAMF carries.
//!
//! LPCM is implemented natively (it is trivial and exercises the trait).
//! Opus, FLAC, and AAC-LC will be adapters over existing pure-Rust codec
//! crates, feature-gated so integrators like Chromium can supply their own
//! decoders instead.

#![forbid(unsafe_code)]

#[cfg(feature = "pcm")]
pub mod pcm;
