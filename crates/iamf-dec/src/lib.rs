//! IAMF decoding pipeline, mirroring the stages of the libiamf reference
//! decoder: codec decode → element reconstruction (demixing/recon gain) →
//! rendering → mixing → post-processing (loudness, peak limiting).
//!
//! The codec layer is pluggable via [`SubstreamDecoder`] so integrators can
//! supply pure-Rust decoders, FFI decoders, or platform decoders.

#![forbid(unsafe_code)]

pub mod element;
pub mod layout;
pub mod params;

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
/// one factory per codec they support. `channels` is the substream's channel
/// count (1, or 2 for coupled substreams), derived from the audio element.
pub trait CodecFactory {
    fn supports(&self, config: &CodecConfig) -> bool;
    fn create(
        &self,
        config: &CodecConfig,
        channels: u8,
    ) -> Result<Box<dyn SubstreamDecoder>, DecodeError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// No registered factory supports the stream's codec config.
    UnsupportedCodec,
    /// The codec rejected a packet.
    CorruptPacket(String),
    /// Descriptors are inconsistent (e.g. substream counts disagree).
    InvalidDescriptors(String),
    /// Pipeline stage not yet implemented in this milestone.
    Unimplemented(&'static str),
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DecodeError::UnsupportedCodec => write!(f, "unsupported codec"),
            DecodeError::CorruptPacket(msg) => write!(f, "corrupt packet: {msg}"),
            DecodeError::InvalidDescriptors(msg) => write!(f, "invalid descriptors: {msg}"),
            DecodeError::Unimplemented(what) => write!(f, "not yet implemented: {what}"),
        }
    }
}

impl std::error::Error for DecodeError {}

// TODO(milestone 4): element reconstructor (demixing, recon gain), renderer
// (channel layouts first, ambisonics after), mixer, loudness normalization,
// peak limiter — see libiamf/code/src/iamf_dec/{iamf_element_reconstructor,
// iamf_renderer,iamf_post_processor,audio_effect_peak_limiter}.c.
