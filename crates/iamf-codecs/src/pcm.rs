//! LPCM substream decoder (IAMF §3.6.4 ipcm).

use iamf_dec::{CodecFactory, DecodeError, DecodedFrame, SubstreamDecoder};
use iamf_obu::descriptors::{CodecConfig, CodecId, DecoderConfig};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    S16,
    S24,
    S32,
}

impl SampleFormat {
    fn bytes_per_sample(self) -> usize {
        match self {
            SampleFormat::S16 => 2,
            SampleFormat::S24 => 3,
            SampleFormat::S32 => 4,
        }
    }
}

/// Decodes interleaved little- or big-endian integer PCM to f32.
///
/// IAMF's `sample_format_flags` selects endianness; sample size is 16, 24,
/// or 32 bits (IAMF §3.6.4).
#[derive(Debug)]
pub struct PcmDecoder {
    format: SampleFormat,
    little_endian: bool,
    channels: u8,
    sample_rate: u32,
}

impl PcmDecoder {
    pub fn new(format: SampleFormat, little_endian: bool, channels: u8, sample_rate: u32) -> Self {
        Self {
            format,
            little_endian,
            channels,
            sample_rate,
        }
    }

    fn sample_to_f32(&self, bytes: &[u8]) -> f32 {
        let value = match (self.format, self.little_endian) {
            (SampleFormat::S16, true) => i32::from(i16::from_le_bytes([bytes[0], bytes[1]])) << 16,
            (SampleFormat::S16, false) => i32::from(i16::from_be_bytes([bytes[0], bytes[1]])) << 16,
            (SampleFormat::S24, true) => i32::from_le_bytes([0, bytes[0], bytes[1], bytes[2]]),
            (SampleFormat::S24, false) => i32::from_be_bytes([bytes[0], bytes[1], bytes[2], 0]),
            (SampleFormat::S32, true) => {
                i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
            }
            (SampleFormat::S32, false) => {
                i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
            }
        };
        value as f32 / -(i32::MIN as f32)
    }
}

impl SubstreamDecoder for PcmDecoder {
    fn decode(&mut self, packet: &[u8], out: &mut DecodedFrame) -> Result<(), DecodeError> {
        let frame_bytes = self.format.bytes_per_sample() * usize::from(self.channels);
        if packet.len() % frame_bytes != 0 {
            return Err(DecodeError::CorruptPacket(format!(
                "packet length {} not a multiple of frame size {frame_bytes}",
                packet.len()
            )));
        }
        out.samples.clear();
        out.samples.extend(
            packet
                .chunks_exact(self.format.bytes_per_sample())
                .map(|bytes| self.sample_to_f32(bytes)),
        );
        out.channels = self.channels;
        out.sample_rate = self.sample_rate;
        Ok(())
    }

    fn reset(&mut self) {}
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PcmFactory;

impl CodecFactory for PcmFactory {
    fn supports(&self, config: &CodecConfig) -> bool {
        config.codec_id == CodecId::Lpcm
            && matches!(
                &config.decoder_config,
                DecoderConfig::Lpcm {
                    sample_size: 16 | 24 | 32,
                    ..
                }
            )
    }

    fn create(
        &self,
        config: &CodecConfig,
        channels: u8,
    ) -> Result<Box<dyn SubstreamDecoder>, DecodeError> {
        let DecoderConfig::Lpcm {
            little_endian,
            sample_size,
            sample_rate,
        } = config.decoder_config
        else {
            return Err(DecodeError::UnsupportedCodec);
        };
        let format = match sample_size {
            16 => SampleFormat::S16,
            24 => SampleFormat::S24,
            32 => SampleFormat::S32,
            _ => return Err(DecodeError::UnsupportedCodec),
        };
        Ok(Box::new(PcmDecoder::new(
            format,
            little_endian,
            channels,
            sample_rate,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(format: SampleFormat, le: bool, packet: &[u8]) -> Vec<f32> {
        let mut decoder = PcmDecoder::new(format, le, 1, 48000);
        let mut frame = DecodedFrame::default();
        decoder.decode(packet, &mut frame).unwrap();
        frame.samples
    }

    #[test]
    fn s16_le_full_scale() {
        let samples = decode(
            SampleFormat::S16,
            true,
            &[0x00, 0x80, 0xff, 0x7f, 0x00, 0x00],
        );
        assert_eq!(samples[0], -1.0);
        assert!((samples[1] - 1.0).abs() < 1e-4);
        assert_eq!(samples[2], 0.0);
    }

    #[test]
    fn s24_be_negative() {
        // 0x800000 = most negative 24-bit value.
        let samples = decode(SampleFormat::S24, false, &[0x80, 0x00, 0x00]);
        assert_eq!(samples[0], -1.0);
    }

    #[test]
    fn s32_le_roundtrip() {
        let samples = decode(SampleFormat::S32, true, &i32::MIN.to_le_bytes());
        assert_eq!(samples[0], -1.0);
    }

    #[test]
    fn misaligned_packet_rejected() {
        let mut decoder = PcmDecoder::new(SampleFormat::S16, true, 2, 48000);
        let mut frame = DecodedFrame::default();
        assert!(decoder.decode(&[0u8; 5], &mut frame).is_err());
    }
}
