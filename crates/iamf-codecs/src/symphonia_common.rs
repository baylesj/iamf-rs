//! Shared adapter: drives a symphonia `Decoder` as an IAMF
//! [`SubstreamDecoder`] (one raw codec frame per IAMF audio frame OBU).

use iamf_dec::{DecodeError, DecodedFrame, SubstreamDecoder};
use symphonia_core::audio::SampleBuffer;
use symphonia_core::codecs::Decoder;
use symphonia_core::formats::Packet;

pub struct SymphoniaSubstreamDecoder<D: Decoder> {
    decoder: D,
    channels: u8,
    sample_rate: u32,
    scratch: Option<SampleBuffer<f32>>,
}

impl<D: Decoder> SymphoniaSubstreamDecoder<D> {
    pub fn new(decoder: D, channels: u8, sample_rate: u32) -> Self {
        Self {
            decoder,
            channels,
            sample_rate,
            scratch: None,
        }
    }
}

impl<D: Decoder> SubstreamDecoder for SymphoniaSubstreamDecoder<D> {
    fn decode(&mut self, packet: &[u8], out: &mut DecodedFrame) -> Result<(), DecodeError> {
        let packet = Packet::new_from_slice(0, 0, 0, packet);
        let decoded = self
            .decoder
            .decode(&packet)
            .map_err(|e| DecodeError::CorruptPacket(e.to_string()))?;
        let spec = *decoded.spec();
        let duration = decoded.capacity() as u64;
        let scratch = self
            .scratch
            .get_or_insert_with(|| SampleBuffer::new(duration, spec));
        if scratch.capacity() < decoded.frames() * spec.channels.count() {
            *scratch = SampleBuffer::new(duration, spec);
        }
        scratch.copy_interleaved_ref(decoded);

        out.samples.clear();
        out.samples.extend_from_slice(scratch.samples());
        out.channels = self.channels;
        out.sample_rate = self.sample_rate;
        Ok(())
    }

    fn reset(&mut self) {
        self.decoder.reset();
    }
}
