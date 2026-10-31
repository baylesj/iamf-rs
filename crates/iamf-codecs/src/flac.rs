//! FLAC substream decoder (IAMF §3.6.3), backed by symphonia's pure-Rust
//! FLAC codec. IAMF audio frame OBUs carry raw FLAC frames; the codec is
//! initialized from the codec config's STREAMINFO block.

use iamf_dec::{CodecFactory, DecodeError, DecodedFrame, SubstreamDecoder};
use iamf_obu::descriptors::{CodecConfig, CodecId, DecoderConfig};
use symphonia_core::codecs::{CODEC_TYPE_FLAC, CodecParameters, Decoder, DecoderOptions};

use crate::symphonia_common::SymphoniaSubstreamDecoder;

/// Constructs FLAC substream decoders.
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
        if !(1..=2).contains(&channels) {
            return Err(DecodeError::UnsupportedCodec);
        }
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
        Ok(Box::new(FlacSubstreamDecoder {
            inner: SymphoniaSubstreamDecoder::new(decoder, channels, *sample_rate),
            channels,
            block_size: config.num_samples_per_frame,
        }))
    }
}

/// FLAC substream decoder: checks each frame header against the
/// substream before symphonia sees it. symphonia asserts (panics) on a
/// frame whose channel count exceeds the configured one or whose block
/// size exceeds STREAMINFO's maximum.
struct FlacSubstreamDecoder {
    inner: SymphoniaSubstreamDecoder<symphonia_bundle_flac::FlacDecoder>,
    channels: u8,
    block_size: u32,
}

impl SubstreamDecoder for FlacSubstreamDecoder {
    fn decode(&mut self, packet: &[u8], out: &mut DecodedFrame) -> Result<(), DecodeError> {
        let header = FrameHeader::parse(packet)
            .ok_or_else(|| DecodeError::CorruptPacket("invalid FLAC frame header".into()))?;
        // §3.6.3: Block Size Bits SHALL be num_samples_per_frame, and the
        // channel bits SHALL match the substream (mono or stereo).
        if header.channels != self.channels || header.block_size != self.block_size {
            return Err(DecodeError::CorruptPacket(format!(
                "FLAC frame of {} samples x {} channels in a {}-sample, {}-channel substream",
                header.block_size, header.channels, self.block_size, self.channels
            )));
        }
        self.inner.decode(packet, out)
    }

    fn reset(&mut self) {
        self.inner.reset();
    }
}

/// The fields of a FLAC frame header (RFC 9639 §9.1) the adapter checks.
#[derive(Debug, PartialEq, Eq)]
struct FrameHeader {
    block_size: u32,
    channels: u8,
}

impl FrameHeader {
    fn parse(frame: &[u8]) -> Option<Self> {
        // Sync code 0b1111_1111_1111_10, reserved 0, blocking strategy.
        if frame.len() < 4 || frame[0] != 0xff || frame[1] & 0xfe != 0xf8 {
            return None;
        }
        let block_size_bits = frame[2] >> 4;
        let channels = match frame[3] >> 4 {
            n @ 0..=7 => n + 1,
            8..=10 => 2, // left/side, side/right, mid/side
            _ => return None,
        };
        // Coded frame or sample number: UTF-8-like, 1 to 7 bytes.
        let lead = *frame.get(4)?;
        let coded_len = match lead.leading_ones() {
            0 => 1,
            n @ 2..=7 => n as usize,
            _ => return None,
        };
        let mut pos = 4 + coded_len;
        let block_size = match block_size_bits {
            0b0001 => 192,
            n @ 0b0010..=0b0101 => 144 << n,
            0b0110 => {
                pos += 1;
                u32::from(*frame.get(pos - 1)?) + 1
            }
            0b0111 => {
                pos += 2;
                u32::from(u16::from_be_bytes([
                    *frame.get(pos - 2)?,
                    *frame.get(pos - 1)?,
                ])) + 1
            }
            n @ 0b1000..=0b1111 => 1 << n,
            _ => return None,
        };
        Some(FrameHeader {
            block_size,
            channels,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_header_fields() {
        // 16-bit block size field (4096 - 1), stereo (channel bits 1).
        let frame = [0xff, 0xf8, 0x79, 0x18, 0x00, 0x0f, 0xff, 0x00];
        assert_eq!(
            FrameHeader::parse(&frame),
            Some(FrameHeader {
                block_size: 4096,
                channels: 2
            })
        );
        // 8-bit field (64 - 1), mono; mid/side stereo; table sizes.
        assert_eq!(
            FrameHeader::parse(&[0xff, 0xf8, 0x69, 0x08, 0x00, 0x3f, 0x00]).map(|h| h.block_size),
            Some(64)
        );
        assert_eq!(
            FrameHeader::parse(&[0xff, 0xf8, 0x19, 0xa8, 0x00]).map(|h| (h.block_size, h.channels)),
            Some((192, 2))
        );
        assert_eq!(
            FrameHeader::parse(&[0xff, 0xf8, 0xc9, 0x08, 0x00]).map(|h| h.block_size),
            Some(4096)
        );
        // Bad sync, reserved channel bits, truncated size field.
        assert_eq!(FrameHeader::parse(&[0xff, 0xf0, 0x19, 0x08, 0x00]), None);
        assert_eq!(FrameHeader::parse(&[0xff, 0xf8, 0x19, 0xb8, 0x00]), None);
        assert_eq!(
            FrameHeader::parse(&[0xff, 0xf8, 0x79, 0x08, 0x00, 0x0f]),
            None
        );
    }
}
