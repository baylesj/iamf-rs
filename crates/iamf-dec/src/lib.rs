//! IAMF decoding pipeline, mirroring the stages of the libiamf reference
//! decoder: codec decode → element reconstruction (demixing/recon gain) →
//! rendering → mixing → post-processing (loudness, peak limiting).
//!
//! The codec layer is pluggable via [`SubstreamDecoder`] so integrators can
//! supply pure-Rust decoders, FFI decoders, or platform decoders.

#![forbid(unsafe_code)]

pub mod layout;

use iamf_obu::descriptors::CodecConfig;

/// PCM output of one substream for one frame: interleaved f32 in [-1, 1].
#[derive(Debug, Clone, Default)]
pub struct DecodedFrame {
    pub samples: Vec<f32>,
    pub channels: u8,
    pub sample_rate: u32,
}

/// Decodes the coded frames of a single IAMF substream (mono or coupled
/// stereo) to PCM. One instance per substream; implementations hold codec
/// state across frames.
pub trait SubstreamDecoder {
    /// Decodes one audio frame OBU payload.
    fn decode(&mut self, packet: &[u8], out: &mut DecodedFrame) -> Result<(), DecodeError>;

    /// Resets codec state (seek/discontinuity).
    fn reset(&mut self);
}

/// Constructs [`SubstreamDecoder`]s for a codec config. Integrators register
/// one factory per codec they support.
pub trait CodecFactory {
    fn supports(&self, config: &CodecConfig) -> bool;
    fn create(&self, config: &CodecConfig) -> Result<Box<dyn SubstreamDecoder>, DecodeError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// No registered factory supports the stream's codec config.
    UnsupportedCodec,
    /// The codec rejected a packet.
    CorruptPacket(String),
    /// Pipeline stage not yet implemented in this milestone.
    Unimplemented(&'static str),
}

// TODO(milestone 3): element reconstructor (demixing, recon gain).
// TODO(milestone 4): renderer (channel layouts first, ambisonics after),
// mixer, loudness normalization, peak limiter — see
// libiamf/code/src/iamf_dec/{iamf_element_reconstructor,iamf_renderer,
// iamf_post_processor,audio_effect_peak_limiter}.c for reference behavior.
