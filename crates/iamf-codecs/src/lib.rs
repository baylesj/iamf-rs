//! [`iamf_dec::SubstreamDecoder`] implementations for the codecs IAMF
//! carries, each behind a feature gate so integrators like Chromium can
//! supply their own decoders instead.

#![forbid(unsafe_code)]

#[cfg(feature = "aac")]
pub mod aac;
#[cfg(feature = "flac")]
pub mod flac;
#[cfg(feature = "opus")]
pub mod opus;
#[cfg(feature = "opus-ffi")]
pub mod opus_ffi;
#[cfg(feature = "pcm")]
pub mod pcm;
#[cfg(any(feature = "flac", feature = "aac"))]
mod symphonia_common;

use iamf_dec::{CodecFactory, DecodeError, SubstreamDecoder};
use iamf_obu::descriptors::CodecConfig;

/// Factory over every codec enabled at build time.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultFactory;

impl CodecFactory for DefaultFactory {
    fn supports(&self, config: &CodecConfig) -> bool {
        #[cfg(feature = "pcm")]
        if pcm::PcmFactory.supports(config) {
            return true;
        }
        #[cfg(feature = "opus-ffi")]
        if opus_ffi::OpusFfiFactory.supports(config) {
            return true;
        }
        #[cfg(feature = "opus")]
        if opus::OpusFactory.supports(config) {
            return true;
        }
        #[cfg(feature = "flac")]
        if flac::FlacFactory.supports(config) {
            return true;
        }
        #[cfg(feature = "aac")]
        if aac::AacFactory.supports(config) {
            return true;
        }
        let _ = config;
        false
    }

    fn create(
        &self,
        config: &CodecConfig,
        channels: u8,
    ) -> Result<Box<dyn SubstreamDecoder>, DecodeError> {
        #[cfg(feature = "pcm")]
        if pcm::PcmFactory.supports(config) {
            return pcm::PcmFactory.create(config, channels);
        }
        #[cfg(feature = "opus-ffi")]
        if opus_ffi::OpusFfiFactory.supports(config) {
            return opus_ffi::OpusFfiFactory.create(config, channels);
        }
        #[cfg(feature = "opus")]
        if opus::OpusFactory.supports(config) {
            return opus::OpusFactory.create(config, channels);
        }
        #[cfg(feature = "flac")]
        if flac::FlacFactory.supports(config) {
            return flac::FlacFactory.create(config, channels);
        }
        #[cfg(feature = "aac")]
        if aac::AacFactory.supports(config) {
            return aac::AacFactory.create(config, channels);
        }
        let _ = (config, channels);
        Err(DecodeError::UnsupportedCodec)
    }
}
