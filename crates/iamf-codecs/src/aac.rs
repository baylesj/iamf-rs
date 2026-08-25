//! AAC-LC substream decoder (IAMF §3.6.2), backed by symphonia's
//! pure-Rust AAC codec. IAMF audio frame OBUs carry raw AAC frames; the
//! codec is initialized from the AudioSpecificConfig in the codec config.

use iamf_dec::{CodecFactory, DecodeError, SubstreamDecoder};
use iamf_obu::descriptors::{CodecConfig, CodecId, DecoderConfig};
use symphonia_core::codecs::{CODEC_TYPE_AAC, CodecParameters, Decoder, DecoderOptions};

use crate::symphonia_common::SymphoniaSubstreamDecoder;

/// Sample rate table for AudioSpecificConfig samplingFrequencyIndex.
const SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

fn asc_sample_rate(asc: &[u8]) -> Option<u32> {
    // audioObjectType (5 bits), samplingFrequencyIndex (4 bits).
    let bits = u32::from_be_bytes([
        *asc.first()?,
        asc.get(1).copied().unwrap_or(0),
        asc.get(2).copied().unwrap_or(0),
        asc.get(3).copied().unwrap_or(0),
    ]);
    let index = (bits >> 23) & 0x0f;
    if index == 0x0f {
        // Explicit 24-bit frequency.
        Some((bits >> 7) & 0xff_ffff)
    } else {
        SAMPLE_RATES.get(index as usize).copied()
    }
}

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
        let DecoderConfig::AacLc {
            audio_specific_config,
        } = &config.decoder_config
        else {
            return Err(DecodeError::UnsupportedCodec);
        };
        let sample_rate = asc_sample_rate(audio_specific_config)
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
