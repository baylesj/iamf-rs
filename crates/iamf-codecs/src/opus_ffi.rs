//! Opus substream decoder backed by libopus via the safe `opus` bindings
//! crate. For integrators that already ship libopus (e.g. Chromium) or
//! want its hand-tuned SIMD; the pure-Rust path lives in [`crate::opus`].

use iamf_dec::{CodecFactory, DecodeError, DecodedFrame, SubstreamDecoder};
use iamf_obu::descriptors::{CodecConfig, CodecId, DecoderConfig};

const SAMPLE_RATE: u32 = 48_000;
/// Largest possible Opus frame: 120 ms at 48 kHz.
const MAX_FRAME_SAMPLES: usize = 5760;

pub struct OpusFfiSubstreamDecoder {
    decoder: ::opus::Decoder,
    channels: u8,
    buffer: Vec<f32>,
}

impl OpusFfiSubstreamDecoder {
    pub fn new(channels: u8) -> Result<Self, DecodeError> {
        let ch = match channels {
            1 => ::opus::Channels::Mono,
            2 => ::opus::Channels::Stereo,
            _ => return Err(DecodeError::UnsupportedCodec),
        };
        let decoder = ::opus::Decoder::new(SAMPLE_RATE, ch)
            .map_err(|e| DecodeError::CorruptPacket(format!("libopus init: {e}")))?;
        Ok(Self {
            decoder,
            channels,
            buffer: vec![0.0; MAX_FRAME_SAMPLES * usize::from(channels)],
        })
    }
}

impl SubstreamDecoder for OpusFfiSubstreamDecoder {
    fn decode(&mut self, packet: &[u8], out: &mut DecodedFrame) -> Result<(), DecodeError> {
        let samples_per_channel = self
            .decoder
            .decode_float(packet, &mut self.buffer, false)
            .map_err(|e| DecodeError::CorruptPacket(format!("libopus: {e}")))?;
        let len = samples_per_channel * usize::from(self.channels);
        out.samples.clear();
        out.samples.extend_from_slice(&self.buffer[..len]);
        out.channels = self.channels;
        out.sample_rate = SAMPLE_RATE;
        Ok(())
    }

    fn reset(&mut self) {
        if let Ok(fresh) = Self::new(self.channels) {
            self.decoder = fresh.decoder;
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct OpusFfiFactory;

impl CodecFactory for OpusFfiFactory {
    fn supports(&self, config: &CodecConfig) -> bool {
        config.codec_id == CodecId::Opus
            && matches!(&config.decoder_config, DecoderConfig::Opus { version, .. } if *version <= 15)
    }

    fn create(
        &self,
        config: &CodecConfig,
        channels: u8,
    ) -> Result<Box<dyn SubstreamDecoder>, DecodeError> {
        if !self.supports(config) || !(1..=2).contains(&channels) {
            return Err(DecodeError::UnsupportedCodec);
        }
        Ok(Box::new(OpusFfiSubstreamDecoder::new(channels)?))
    }
}
