//! AAC-LC substream decoder (IAMF §3.6.2), backed by symphonia's
//! pure-Rust AAC codec. IAMF audio frame OBUs carry raw AAC frames; the
//! codec is initialized from the AudioSpecificConfig in the codec config.

use iamf_dec::{CodecFactory, DecodeError, SubstreamDecoder};
use iamf_obu::descriptors::{CodecConfig, CodecId, DecoderConfig};
use symphonia_core::codecs::{CODEC_TYPE_AAC, CodecParameters, Decoder, DecoderOptions};

use crate::symphonia_common::SymphoniaSubstreamDecoder;

/// Constructs AAC-LC substream decoders.
#[derive(Debug, Clone, Copy, Default)]
pub struct AacFactory;

impl CodecFactory for AacFactory {
    fn supports(&self, config: &CodecConfig) -> bool {
        config.codec_id == CodecId::AacLc
            && matches!(&config.decoder_config, DecoderConfig::AacLc { .. })
    }

    fn create(
        &self,
        config: &CodecConfig,
        channels: u8,
    ) -> Result<Box<dyn SubstreamDecoder>, DecodeError> {
        if !(1..=2).contains(&channels) {
            return Err(DecodeError::UnsupportedCodec);
        }
        let DecoderConfig::AacLc {
            audio_specific_config,
        } = &config.decoder_config
        else {
            return Err(DecodeError::UnsupportedCodec);
        };
        let sample_rate = iamf_dec::profile::output_sample_rate(config)
            .ok_or(DecodeError::CorruptPacket("bad AudioSpecificConfig".into()))?;
        // Patch the channelConfiguration (4 bits after the frequency
        // index) to this substream's width, as with FLAC.
        let mut asc = audio_specific_config.clone();
        if asc.len() >= 2 && (asc[0] & 0x07) != 0x07 {
            // channelConfiguration occupies bits 9..=12 for ordinary
            // frequency indices.
            asc[1] = (asc[1] & !0x78) | (channels << 3);
        }
        let mut params = CodecParameters::new();
        params
            .for_codec(CODEC_TYPE_AAC)
            .with_sample_rate(sample_rate)
            .with_extra_data(asc.into_boxed_slice());
        let decoder = symphonia_codec_aac::AacDecoder::try_new(&params, &DecoderOptions::default())
            .map_err(|e| DecodeError::CorruptPacket(format!("aac init: {e}")))?;
        Ok(Box::new(SymphoniaSubstreamDecoder::new(
            decoder,
            channels,
            sample_rate,
        )))
    }
}
