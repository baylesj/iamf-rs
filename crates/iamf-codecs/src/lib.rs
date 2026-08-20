//! [`iamf_dec::SubstreamDecoder`] implementations for the codecs IAMF
//! carries, each behind a feature gate so integrators like Chromium can
//! supply their own decoders instead.

#![forbid(unsafe_code)]

#[cfg(feature = "opus")]
pub mod opus;
#[cfg(feature = "pcm")]
pub mod pcm;

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
        #[cfg(feature = "opus")]
        if opus::OpusFactory.supports(config) {
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
        #[cfg(feature = "opus")]
        if opus::OpusFactory.supports(config) {
            return opus::OpusFactory.create(config, channels);
        }
        let _ = (config, channels);
        Err(DecodeError::UnsupportedCodec)
    }
}
