//! Descriptor OBU payloads: codec config, audio element, mix presentation,
//! and the IA sequence header.
//!
//! Milestone 2: parse these from [`crate::Obu`] payloads. The structures
//! below sketch the spec model (IAMF §3.6–§3.9) and will grow field-by-field
//! alongside conformance tests against libiamf.

/// 4CC identifying the codec of a substream (IAMF §3.6.1 codec_id).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecId {
    /// 'Opus'
    Opus,
    /// 'mp4a' (AAC-LC)
    AacLc,
    /// 'fLaC'
    Flac,
    /// 'ipcm'
    Lpcm,
    /// Unrecognized 4CC, preserved for diagnostics.
    Unknown([u8; 4]),
}

impl CodecId {
    pub fn from_fourcc(fourcc: [u8; 4]) -> Self {
        match &fourcc {
            b"Opus" => CodecId::Opus,
            b"mp4a" => CodecId::AacLc,
            b"fLaC" => CodecId::Flac,
            b"ipcm" => CodecId::Lpcm,
            _ => CodecId::Unknown(fourcc),
        }
    }
}

/// IA sequence header OBU payload (IAMF §3.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceHeader {
    pub ia_code: u32,
    pub primary_profile: u8,
    pub additional_profile: u8,
}

/// Codec config OBU payload (IAMF §3.6). TODO(milestone 2): parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecConfig {
    pub codec_config_id: u32,
    pub codec_id: CodecId,
    pub num_samples_per_frame: u32,
    pub audio_roll_distance: i16,
    /// Codec-specific decoder config (e.g. OpusHead-equivalent fields).
    pub decoder_config: Vec<u8>,
}

/// Audio element OBU payload (IAMF §3.7). TODO(milestone 2): parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioElement {
    pub audio_element_id: u32,
    pub codec_config_id: u32,
    pub substream_ids: Vec<u32>,
}

/// Mix presentation OBU payload (IAMF §3.8). TODO(milestone 2): parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MixPresentation {
    pub mix_presentation_id: u32,
}
