//! Opus substream decoder (IAMF §3.6.1), backed by the pure-Rust
//! `opus-decoder` crate (RFC 8251 conformant, no unsafe, no FFI).
//!
//! IAMF timing is defined at 48 kHz for Opus streams, so substreams are
//! always decoded at 48 kHz regardless of the encoder's input rate.

use iamf_dec::{CodecFactory, DecodeError, DecodedFrame, SubstreamDecoder};
use iamf_obu::descriptors::CodecConfig;

use crate::opus_common::{MAX_FRAME_SAMPLES, SAMPLE_RATE};

impl core::fmt::Debug for OpusSubstreamDecoder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OpusSubstreamDecoder")
            .field("channels", &self.channels)
            .finish_non_exhaustive()
    }
}

/// Pure-Rust Opus substream decoder backed by `opus-decoder`.
pub struct OpusSubstreamDecoder {
    decoder: opus_decoder::OpusDecoder,
    channels: u8,
    buffer: Vec<f32>,
}

impl OpusSubstreamDecoder {
    /// Creates a new pure-Rust Opus decoder for the given channel count.
    pub fn new(channels: u8) -> Result<Self, DecodeError> {
        crate::opus_common::validate_channels(channels)?;
        let decoder = opus_decoder::OpusDecoder::new(SAMPLE_RATE, channels.into())
            .map_err(|e| DecodeError::CorruptPacket(format!("opus init: {e:?}")))?;
        Ok(Self {
            decoder,
            channels,
            buffer: vec![0.0; MAX_FRAME_SAMPLES * usize::from(channels)],
        })
    }
}

impl SubstreamDecoder for OpusSubstreamDecoder {
    fn decode(&mut self, packet: &[u8], out: &mut DecodedFrame) -> Result<(), DecodeError> {
        // Guard against arithmetic panics in the upstream `opus-decoder`
        // crate on adversarial bitstreams (e.g. `extract_collapse_mask`
        // shift overflow when overflow-checks are enabled).
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.decoder.decode_float(packet, &mut self.buffer, false)
        }));
        let samples_per_channel = match res {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(DecodeError::CorruptPacket(format!("opus: {e:?}"))),
            Err(_) => {
                self.reset();
                return Err(DecodeError::CorruptPacket("opus: decoder panic".into()));
            }
        };
        crate::opus_common::emit(&self.buffer, samples_per_channel, self.channels, out);
        Ok(())
    }

    fn reset(&mut self) {
        // Recreate rather than carry decoder state across a discontinuity.
        if let Ok(fresh) = Self::new(self.channels) {
            self.decoder = fresh.decoder;
        }
    }
}

/// Constructs pure-Rust Opus substream decoders.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpusFactory;

impl CodecFactory for OpusFactory {
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
        Ok(Box::new(OpusSubstreamDecoder::new(channels)?))
    }
}
