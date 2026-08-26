//! Shared plumbing for the two Opus adapters ([`crate::opus`] pure-Rust,
//! [`crate::opus_ffi`] libopus), so config validation and output shaping
//! cannot drift between them.

use iamf_dec::{DecodeError, DecodedFrame};
use iamf_obu::descriptors::{CodecConfig, CodecId, DecoderConfig};

/// IAMF timing is defined at 48 kHz for Opus streams, so substreams are
/// always decoded at 48 kHz regardless of the encoder's input rate.
pub(crate) const SAMPLE_RATE: u32 = 48_000;

/// Largest possible Opus frame: 120 ms at 48 kHz.
pub(crate) const MAX_FRAME_SAMPLES: usize = 5760;

/// §3.6.1 requires `version == 1`; like RFC 7845 readers, versions up to
/// 15 are treated as backwards compatible.
pub(crate) fn supports(config: &CodecConfig) -> bool {
    config.codec_id == CodecId::Opus
        && matches!(&config.decoder_config, DecoderConfig::Opus { version, .. } if *version <= 15)
}

/// IAMF substreams are mono or coupled stereo only.
pub(crate) fn validate_channels(channels: u8) -> Result<(), DecodeError> {
    if (1..=2).contains(&channels) {
        Ok(())
    } else {
        Err(DecodeError::UnsupportedCodec)
    }
}

/// Copies one decoded frame out of the scratch buffer.
pub(crate) fn emit(
    buffer: &[f32],
    samples_per_channel: usize,
    channels: u8,
    out: &mut DecodedFrame,
) {
    let len = samples_per_channel * usize::from(channels);
    out.samples.clear();
    out.samples.extend_from_slice(&buffer[..len]);
    out.channels = channels;
    out.sample_rate = SAMPLE_RATE;
}
