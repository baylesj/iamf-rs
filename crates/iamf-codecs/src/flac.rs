//! FLAC substream decoder (IAMF §3.6.3), backed by symphonia's pure-Rust
//! FLAC codec. IAMF audio frame OBUs carry raw FLAC frames; the codec is
//! initialized from the codec config's STREAMINFO block.

use iamf_dec::{CodecFactory, DecodeError, SubstreamDecoder};
use iamf_obu::descriptors::{CodecConfig, CodecId, DecoderConfig};
use symphonia_core::codecs::{CODEC_TYPE_FLAC, CodecParameters, Decoder, DecoderOptions};

use crate::symphonia_common::SymphoniaSubstreamDecoder;

#[derive(Debug, Clone, Copy, Default)]
pub struct FlacFactory;

impl CodecFactory for FlacFactory {
    fn supports(&self, config: &CodecConfig) -> bool {
        config.codec_id == CodecId::Flac
            && matches!(&config.decoder_config, DecoderConfig::Flac { .. })
    }

    fn create(
        &self,
        config: &CodecConfig,
        channels: u8,
    ) -> Result<Box<dyn SubstreamDecoder>, DecodeError> {
        let DecoderConfig::Flac {
            sample_rate,
            streaminfo,
            ..
        } = &config.decoder_config
        else {
            return Err(DecodeError::UnsupportedCodec);
        };
        // The codec-config STREAMINFO describes the whole element; each
        // substream is mono or stereo, so patch the channel count field
        // (bits 100..103 of the block) to this substream's width.
        let mut streaminfo = streaminfo.clone();
        if streaminfo.len() >= 13 {
            streaminfo[12] = (streaminfo[12] & !0x0e) | ((channels - 1) << 1);
        }
        let mut params = CodecParameters::new();
        params
            .for_codec(CODEC_TYPE_FLAC)
            .with_sample_rate(*sample_rate)
            .with_extra_data(streaminfo.into_boxed_slice());
        let decoder =
            symphonia_bundle_flac::FlacDecoder::try_new(&params, &DecoderOptions::default())
                .map_err(|e| DecodeError::CorruptPacket(format!("flac init: {e}")))?;
        Ok(Box::new(SymphoniaSubstreamDecoder::new(
            decoder,
            channels,
            *sample_rate,
        )))
    }
}
