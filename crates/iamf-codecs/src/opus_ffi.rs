//! Opus substream decoder backed by an external libopus (system or
//! integrator-provided, see the `iamf-opus-ffi` crate). For integrators
//! that already ship libopus (e.g. Chromium) or want its hand-tuned SIMD;
//! the pure-Rust path lives in [`crate::opus`].

use iamf_dec::{CodecFactory, DecodeError, DecodedFrame, SubstreamDecoder};
use iamf_obu::descriptors::CodecConfig;

use crate::opus_common::{MAX_FRAME_SAMPLES, SAMPLE_RATE};

pub struct OpusFfiSubstreamDecoder {
    decoder: iamf_opus_ffi::Decoder,
    channels: u8,
    buffer: Vec<f32>,
}

impl OpusFfiSubstreamDecoder {
    pub fn new(channels: u8) -> Result<Self, DecodeError> {
        crate::opus_common::validate_channels(channels)?;
        let decoder = iamf_opus_ffi::Decoder::new(SAMPLE_RATE, usize::from(channels))
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
            .decode_float(packet, &mut self.buffer)
            .map_err(|e| DecodeError::CorruptPacket(format!("libopus: {e}")))?;
        crate::opus_common::emit(&self.buffer, samples_per_channel, self.channels, out);
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
        crate::opus_common::supports(config)
    }

    fn create(
        &self,
        config: &CodecConfig,
        channels: u8,
    ) -> Result<Box<dyn SubstreamDecoder>, DecodeError> {
        if !self.supports(config) {
            return Err(DecodeError::UnsupportedCodec);
        }
        Ok(Box::new(OpusFfiSubstreamDecoder::new(channels)?))
    }
}
