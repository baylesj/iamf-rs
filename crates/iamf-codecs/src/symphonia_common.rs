//! Shared adapter: drives a symphonia `AudioDecoder` as an IAMF
//! [`SubstreamDecoder`] (one raw codec frame per IAMF audio frame OBU).

use iamf_dec::{DecodeError, DecodedFrame, SubstreamDecoder};
use symphonia_core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia_core::packet::PacketRef;
use symphonia_core::units::{Duration, Timestamp};

/// Options for every symphonia decoder we create. Gapless trimming is off:
/// IAMF trims with the audio frame OBU's `num_samples_to_trim_*` fields,
/// so the codec must return every decoded frame, priming included.
pub(crate) fn decoder_options() -> AudioDecoderOptions {
    let mut options = AudioDecoderOptions::default();
    options.gapless = false;
    options
}

pub(crate) struct SymphoniaSubstreamDecoder<D: AudioDecoder> {
    decoder: D,
    channels: u8,
    sample_rate: u32,
}

impl<D: AudioDecoder> SymphoniaSubstreamDecoder<D> {
    fn decode_unguarded(
        &mut self,
        packet: &[u8],
        out: &mut DecodedFrame,
    ) -> Result<(), DecodeError> {
        let packet = PacketRef::new(0, Timestamp::ZERO, Duration::ZERO, packet);
        let decoded = self
            .decoder
            .decode_ref(&packet)
            .map_err(|e| DecodeError::CorruptPacket(e.to_string()))?;
        decoded.copy_to_vec_interleaved(&mut out.samples);
        out.channels = self.channels;
        out.sample_rate = self.sample_rate;
        Ok(())
    }

    pub(crate) fn new(decoder: D, channels: u8, sample_rate: u32) -> Self {
        Self {
            decoder,
            channels,
            sample_rate,
        }
    }
}

impl<D: AudioDecoder> SubstreamDecoder for SymphoniaSubstreamDecoder<D> {
    fn decode(&mut self, packet: &[u8], out: &mut DecodedFrame) -> Result<(), DecodeError> {
        // symphonia asserts on some malformed packets. The adapters
        // pre-check what they can; this keeps any remaining panic from
        // unwinding into the decoder (it cannot help panic=abort builds).
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.decode_unguarded(packet, out)
        }))
        .unwrap_or_else(|_| {
            self.decoder.reset();
            Err(DecodeError::CorruptPacket("codec panicked".into()))
        })
    }

    fn reset(&mut self) {
        self.decoder.reset();
    }
}
