//! Descriptor OBU payloads: IA sequence header, codec config, audio element,
//! and mix presentation (IAMF v1.1 §3.5–§3.8, plus the IAMF v2.0
//! additions).
//!
//! IAMF v1.1 syntax is parsed exactly as before. IAMF v2.0 additions are
//! parsed too, following iamf-tools v3.0.0 (the reference this crate
//! matches): object-based audio elements, the rendering-config extension
//! (position parameter definitions, [`ElementGainOffsetConfig`]) and
//! [`BinauralFilterProfile`], [`MixPresentationOptionalFields`], live
//! loudness, the Base-Advanced / Advanced profiles ([`ProfileVersion`]) and
//! the 10.2.9.3 / 7.1.5.4 expanded layouts ([`ExpandedLayout`]). Anything
//! still unknown (reserved element types, parameter types, extension
//! bytes) arrives inside sized regions, which are skipped rather than
//! rejected, so newer streams keep parsing.

use crate::position::{PositionParamDefinition, PositionParamType};
use crate::{ByteReader, Error, Obu, ObuType, Result};

/// A parsed descriptor OBU payload.
#[derive(Debug, Clone, PartialEq)]
pub enum Descriptor {
    /// IA sequence header (§3.5).
    SequenceHeader(SequenceHeader),
    /// Codec config (§3.6).
    CodecConfig(CodecConfig),
    /// Audio element (§3.7).
    AudioElement(AudioElement),
    /// Mix presentation (§3.8).
    MixPresentation(MixPresentation),
}

/// Parses the payload of a descriptor OBU. Returns `None` for OBU types that
/// are not descriptors (audio frames, parameter blocks, temporal delimiters,
/// metadata, reserved types).
pub fn parse(obu: &Obu<'_>) -> Result<Option<Descriptor>> {
    let mut r = ByteReader::new(obu.payload);
    let descriptor = match obu.header.obu_type {
        ObuType::SequenceHeader => Descriptor::SequenceHeader(SequenceHeader::parse(&mut r)?),
        ObuType::CodecConfig => Descriptor::CodecConfig(CodecConfig::parse(&mut r)?),
        ObuType::AudioElement => Descriptor::AudioElement(AudioElement::parse(&mut r)?),
        ObuType::MixPresentation => Descriptor::MixPresentation(
            MixPresentation::parse_with_optional_fields(&mut r, obu.header.optional_fields_flag())?,
        ),
        _ => return Ok(None),
    };
    Ok(Some(descriptor))
}

fn invalid(r: &ByteReader<'_>) -> Error {
    Error::InvalidDescriptor {
        offset: r.position(),
    }
}

// ---------------------------------------------------------------------------
// IA sequence header (§3.5)
// ---------------------------------------------------------------------------

/// IA sequence header (§3.5): the profiles the stream complies with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceHeader {
    /// Profile the full stream complies with (see [`ProfileVersion`]).
    pub primary_profile: u8,
    /// A profile the stream also complies with when unsupported elements
    /// are ignored.
    pub additional_profile: u8,
}

impl SequenceHeader {
    /// Parses a sequence header OBU payload (validates the `iamf` 4CC).
    pub fn parse(r: &mut ByteReader<'_>) -> Result<Self> {
        if &r.read_fourcc()? != b"iamf" {
            return Err(invalid(r));
        }
        Ok(SequenceHeader {
            primary_profile: r.read_u8()?,
            additional_profile: r.read_u8()?,
        })
    }

    /// [`SequenceHeader::primary_profile`] as a [`ProfileVersion`].
    pub fn primary(&self) -> ProfileVersion {
        ProfileVersion::from_u8(self.primary_profile)
    }

    /// [`SequenceHeader::additional_profile`] as a [`ProfileVersion`].
    pub fn additional(&self) -> ProfileVersion {
        ProfileVersion::from_u8(self.additional_profile)
    }
}

/// IA sequence header profile values (iamf-tools `ProfileVersion`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ProfileVersion {
    /// 0: Simple (IAMF v1.0).
    Simple,
    /// 1: Base (IAMF v1.0).
    Base,
    /// 2: Base-Enhanced (IAMF v1.1).
    BaseEnhanced,
    /// 3: Base-Advanced (IAMF v2.0).
    BaseAdvanced,
    /// 4: Advanced-1 (IAMF v2.0).
    Advanced1,
    /// 5: Advanced-2 (IAMF v2.0).
    Advanced2,
    /// Any other value (reserved for future profiles).
    Reserved(u8),
}

impl ProfileVersion {
    /// Maps a coded profile value.
    pub fn from_u8(value: u8) -> Self {
        match value {
            0 => ProfileVersion::Simple,
            1 => ProfileVersion::Base,
            2 => ProfileVersion::BaseEnhanced,
            3 => ProfileVersion::BaseAdvanced,
            4 => ProfileVersion::Advanced1,
            5 => ProfileVersion::Advanced2,
            other => ProfileVersion::Reserved(other),
        }
    }

    /// The coded profile value.
    pub fn as_u8(self) -> u8 {
        match self {
            ProfileVersion::Simple => 0,
            ProfileVersion::Base => 1,
            ProfileVersion::BaseEnhanced => 2,
            ProfileVersion::BaseAdvanced => 3,
            ProfileVersion::Advanced1 => 4,
            ProfileVersion::Advanced2 => 5,
            ProfileVersion::Reserved(other) => other,
        }
    }
}

// ---------------------------------------------------------------------------
// Codec config (§3.6)
// ---------------------------------------------------------------------------

/// The codec of a codec config, from its 4CC (§3.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecId {
    /// `Opus`.
    Opus,
    /// `mp4a` (AAC-LC).
    AacLc,
    /// `fLaC`.
    Flac,
    /// `ipcm` (linear PCM).
    Lpcm,
    /// An unrecognized 4CC, preserved for diagnostics.
    Unknown([u8; 4]),
}

impl CodecId {
    /// Maps a codec_id 4CC to a [`CodecId`].
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

/// Codec-specific decoder config (§3.6.1–§3.6.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecoderConfig {
    /// §3.6.1: OpusHead-equivalent fields, but big-endian.
    Opus {
        /// OpusHead version; §3.6.1 requires 1.
        version: u8,
        /// Channels of the element (substreams are mono/stereo regardless).
        output_channel_count: u8,
        /// Encoder look-ahead in 48 kHz samples (informational; trimming
        /// is carried by the audio frames).
        pre_skip: u16,
        /// The encoder's input rate (decode is always at 48 kHz).
        input_sample_rate: u32,
        /// OpusHead output gain, Q7.8 dB (informational).
        output_gain: i16,
        /// OpusHead channel mapping family (informational).
        mapping_family: u8,
    },
    /// §3.6.3: fields from the FLAC STREAMINFO metadata block, plus the
    /// raw 34-byte block body for codec initialization.
    Flac {
        /// From STREAMINFO.
        sample_rate: u32,
        /// From STREAMINFO.
        bits_per_sample: u8,
        /// The raw STREAMINFO block body.
        streaminfo: Vec<u8>,
    },
    /// §3.6.4.
    Lpcm {
        /// sample_format_flags bit 0.
        little_endian: bool,
        /// Bits per sample (16, 24, or 32).
        sample_size: u8,
        /// Samples per second.
        sample_rate: u32,
    },
    /// §3.6.2: the AudioSpecificConfig extracted from the
    /// DecoderConfigDescriptor, for codec initialization.
    AacLc {
        /// The raw AudioSpecificConfig bytes.
        audio_specific_config: Vec<u8>,
    },
    /// Unrecognized codec: the raw decoder_config bytes.
    Unknown(Vec<u8>),
}

/// Codec config descriptor (§3.6): how substreams referencing it are
/// coded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecConfig {
    /// Identifier audio elements reference.
    pub codec_config_id: u32,
    /// The codec, from its 4CC.
    pub codec_id: CodecId,
    /// Samples per audio frame (per channel).
    pub num_samples_per_frame: u32,
    /// Frames needed to converge after a seek (§3.6, e.g. -4 for AAC).
    pub audio_roll_distance: i16,
    /// Codec-specific initialization data.
    pub decoder_config: DecoderConfig,
}

impl CodecConfig {
    /// Parses a codec config OBU payload.
    pub fn parse(r: &mut ByteReader<'_>) -> Result<Self> {
        let codec_config_id = r.read_leb128()?;
        let codec_id = CodecId::from_fourcc(r.read_fourcc()?);
        let num_samples_per_frame = r.read_leb128()?;
        if num_samples_per_frame == 0 {
            return Err(invalid(r));
        }
        let audio_roll_distance = r.read_i16_be()?;
        let decoder_config = match codec_id {
            CodecId::Opus => DecoderConfig::Opus {
                version: r.read_u8()?,
                output_channel_count: r.read_u8()?,
                pre_skip: r.read_u16_be()?,
                input_sample_rate: r.read_u32_be()?,
                output_gain: r.read_i16_be()?,
                mapping_family: r.read_u8()?,
            },
            CodecId::Flac => Self::parse_flac_streaminfo(r)?,
            CodecId::Lpcm => {
                let flags = r.read_u8()?;
                DecoderConfig::Lpcm {
                    little_endian: flags & 0x01 != 0,
                    sample_size: r.read_u8()?,
                    sample_rate: r.read_u32_be()?,
                }
            }
            CodecId::AacLc => Self::parse_aac_decoder_config(r)?,
            CodecId::Unknown(_) => DecoderConfig::Unknown(r.rest().to_vec()),
        };
        Ok(CodecConfig {
            codec_config_id,
            codec_id,
            num_samples_per_frame,
            audio_roll_distance,
            decoder_config,
        })
    }

    /// Walks FLAC metadata blocks to the STREAMINFO block (block type 0).
    fn parse_flac_streaminfo(r: &mut ByteReader<'_>) -> Result<DecoderConfig> {
        loop {
            let header = r.read_u32_be()?;
            let last = header >> 31 & 0x1 != 0;
            let block_type = header >> 24 & 0x7f;
            let length = (header & 0xff_ffff) as usize;
            if block_type == 0 {
                let streaminfo = r.read_bytes(length)?.to_vec();
                if streaminfo.len() < 18 {
                    return Err(invalid(r));
                }
                let packed = u32::from_be_bytes(streaminfo[10..14].try_into().unwrap());
                return Ok(DecoderConfig::Flac {
                    sample_rate: packed >> 12 & 0xf_ffff,
                    bits_per_sample: ((packed >> 4 & 0x1f) + 1) as u8,
                    streaminfo,
                });
            }
            r.skip(length)?;
            if last {
                return Err(invalid(r));
            }
        }
    }

    /// Reads an ISO/IEC 14496-1 expandable length field.
    fn read_expandable(r: &mut ByteReader<'_>) -> Result<usize> {
        let mut size = 0usize;
        for _ in 0..4 {
            let byte = r.read_u8()?;
            size = size << 7 | usize::from(byte & 0x7f);
            if byte & 0x80 == 0 {
                break;
            }
        }
        Ok(size)
    }

    /// §3.6.2: decoder_config is a DecoderConfigDescriptor (tag 0x04):
    /// 13 fixed bytes, then a DecSpecificInfo (tag 0x05) carrying the
    /// AudioSpecificConfig, which codecs need for initialization.
    fn parse_aac_decoder_config(r: &mut ByteReader<'_>) -> Result<DecoderConfig> {
        if r.read_u8()? != 0x04 {
            return Err(invalid(r));
        }
        Self::read_expandable(r)?;
        r.skip(13)?;
        if r.read_u8()? != 0x05 {
            return Err(invalid(r));
        }
        let asc_len = Self::read_expandable(r)?;
        let audio_specific_config = r.read_bytes(asc_len)?.to_vec();
        Ok(DecoderConfig::AacLc {
            audio_specific_config,
        })
    }
}

// ---------------------------------------------------------------------------
// Parameter definitions (§3.6.1 param_definition)
// ---------------------------------------------------------------------------

/// Common parameter definition fields (§3.6.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamDefinition {
    /// The id parameter block OBUs carry to reference this definition.
    pub parameter_id: u32,
    /// Ticks per second of the parameter's time base.
    pub parameter_rate: u32,
    /// Mode 1: parameter blocks define their own timing. Mode 0: timing
    /// below applies.
    pub mode: bool,
    /// Mode-0 block duration in parameter ticks.
    pub duration: u32,
    /// Mode-0 subblock duration; 0 means explicit per-subblock durations.
    pub constant_subblock_duration: u32,
    /// Mode-0 explicit subblock durations (when the above is 0).
    pub subblock_durations: Vec<u32>,
}

impl ParamDefinition {
    #[allow(clippy::redundant_closure_for_method_calls)]
    pub(crate) fn parse(r: &mut ByteReader<'_>) -> Result<Self> {
        let parameter_id = r.read_leb128()?;
        let parameter_rate = r.read_leb128()?;
        let mode = r.read_u8()? & 0x80 != 0;
        let mut def = ParamDefinition {
            parameter_id,
            parameter_rate,
            mode,
            duration: 0,
            constant_subblock_duration: 0,
            subblock_durations: Vec::new(),
        };
        if !mode {
            def.duration = r.read_leb128()?;
            def.constant_subblock_duration = r.read_leb128()?;
            if def.constant_subblock_duration == 0 {
                let count = r.read_leb128()?;
                def.subblock_durations = read_bounded_vec(r, count, |r| r.read_leb128())?;
            }
        }
        Ok(def)
    }
}

/// Parameter definition types (§3.6.1 param_definition_type); mix gain
/// (type 0) never appears in audio elements, only in mix presentations.
const PARAM_TYPE_DEMIXING: u32 = 1;
const PARAM_TYPE_RECON_GAIN: u32 = 2;

/// A parameter declared by an audio element (§3.7 param_definition).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElementParam {
    /// Demixing info (type 1): dynamic down-mix parameters.
    Demixing {
        /// Common definition fields.
        base: ParamDefinition,
        /// dmixp_mode used when no parameter block covers a frame.
        default_demixing_mode: u8,
        /// Initial demixing weight index (w_idx).
        default_weight_index: u8,
    },
    /// Recon gain (type 2), for scalable-channel layers.
    ReconGain(ParamDefinition),
    /// Reserved type; its sized definition was skipped.
    Unknown {
        /// The reserved param_definition_type value.
        param_type: u32,
    },
}

/// Mix gain parameter definition (§3.8 element/output mix config).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MixGainParam {
    /// Common definition fields.
    pub base: ParamDefinition,
    /// Gain applied when no parameter block covers a frame, Q7.8 dB.
    pub default_mix_gain: i16,
}

impl MixGainParam {
    fn parse(r: &mut ByteReader<'_>) -> Result<Self> {
        Ok(MixGainParam {
            base: ParamDefinition::parse(r)?,
            default_mix_gain: r.read_i16_be()?,
        })
    }
}

// ---------------------------------------------------------------------------
// Audio element (§3.7)
// ---------------------------------------------------------------------------

/// One layer of a scalable channel audio config (§3.7.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelAudioLayer {
    /// Layout 0..=9 (mono..binaural) or 15 (expanded).
    pub loudspeaker_layout: u8,
    /// Substreams this layer adds on top of the previous layers.
    pub substream_count: u8,
    /// Of those, how many are coupled (stereo) substreams.
    pub coupled_substream_count: u8,
    /// Whether recon-gain parameter blocks carry data for this layer.
    pub recon_gain_is_present: bool,
    /// (output_gain_flags, output_gain Q7.8 dB) when present.
    pub output_gain: Option<(u8, i16)>,
    /// Present when the first layer's loudspeaker_layout is 15 (expanded);
    /// see [`ExpandedLayout`].
    pub expanded_loudspeaker_layout: Option<u8>,
}

/// §3.7.4 `expanded_loudspeaker_layout` values (iamf-tools
/// `ExpandedLoudspeakerLayout`). 0..=12 arrived with IAMF v1.1
/// (Base-Enhanced); 13..=19 — the 10.2.9.3 (sound system H) and 7.1.5.4
/// families — with IAMF v2.0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExpandedLayout {
    /// 0: LFE of 7.1.4.
    Lfe,
    /// 1: Ls/Rs of 5.1.4.
    StereoS,
    /// 2: Lss/Rss of 7.1.4.
    StereoSs,
    /// 3: Lrs/Rrs of 7.1.4.
    StereoRs,
    /// 4: Ltf/Rtf of 7.1.4.
    StereoTf,
    /// 5: Ltb/Rtb of 7.1.4.
    StereoTb,
    /// 6: Ltf/Rtf/Ltb/Rtb of 7.1.4.
    Top4Ch,
    /// 7: L/C/R of 7.1.4.
    Ch3_0,
    /// 8: 9.1.6 (subset of sound system H).
    Ch9_1_6,
    /// 9: FL/FR of 9.1.6.
    StereoF,
    /// 10: SiL/SiR of 9.1.6.
    StereoSi,
    /// 11: TpSiL/TpSiR of 9.1.6.
    StereoTpSi,
    /// 12: TpFL/TpFR/TpSiL/TpSiR/TpBL/TpBR of 9.1.6.
    Top6Ch,
    /// 13: 10.2.9.3, sound system H (9+10+3) (IAMF v2.0).
    Ch10_2_9_3,
    /// 14: LFE1/LFE2 of 10.2.9.3 (IAMF v2.0).
    LfePair,
    /// 15: BtFL/BtFC/BtFR of 10.2.9.3 (IAMF v2.0).
    Bottom3Ch,
    /// 16: 7.1.5.4: 7.1.4 plus TpC and four bottom channels (IAMF v2.0).
    Ch7_1_5_4,
    /// 17: BtFL/BtFR/BtBL/BtBR of 7.1.5.4 (IAMF v2.0).
    Bottom4Ch,
    /// 18: TpC of 7.1.5.4 (IAMF v2.0).
    Top1Ch,
    /// 19: Ltf/Rtf/Ltb/Rtb/TpC of 7.1.5.4 (IAMF v2.0).
    Top5Ch,
}

impl ExpandedLayout {
    /// Maps a coded value (`None` for reserved values 20..=255).
    pub fn from_u8(value: u8) -> Option<Self> {
        use ExpandedLayout as E;
        const ALL: [ExpandedLayout; 20] = [
            E::Lfe,
            E::StereoS,
            E::StereoSs,
            E::StereoRs,
            E::StereoTf,
            E::StereoTb,
            E::Top4Ch,
            E::Ch3_0,
            E::Ch9_1_6,
            E::StereoF,
            E::StereoSi,
            E::StereoTpSi,
            E::Top6Ch,
            E::Ch10_2_9_3,
            E::LfePair,
            E::Bottom3Ch,
            E::Ch7_1_5_4,
            E::Bottom4Ch,
            E::Top1Ch,
            E::Top5Ch,
        ];
        ALL.get(usize::from(value)).copied()
    }

    /// Number of channels in the layout.
    pub fn channel_count(self) -> usize {
        use ExpandedLayout as E;
        match self {
            E::Lfe | E::Top1Ch => 1,
            E::StereoS
            | E::StereoSs
            | E::StereoRs
            | E::StereoTf
            | E::StereoTb
            | E::StereoF
            | E::StereoSi
            | E::StereoTpSi
            | E::LfePair => 2,
            E::Ch3_0 | E::Bottom3Ch => 3,
            E::Top4Ch | E::Bottom4Ch => 4,
            E::Top5Ch => 5,
            E::Top6Ch => 6,
            E::Ch9_1_6 => 16,
            E::Ch7_1_5_4 => 17,
            E::Ch10_2_9_3 => 24,
        }
    }

    /// Whether the layout arrived with IAMF v2.0 (values 13..=19).
    pub fn is_v2(self) -> bool {
        matches!(
            self,
            ExpandedLayout::Ch10_2_9_3
                | ExpandedLayout::LfePair
                | ExpandedLayout::Bottom3Ch
                | ExpandedLayout::Ch7_1_5_4
                | ExpandedLayout::Bottom4Ch
                | ExpandedLayout::Top1Ch
                | ExpandedLayout::Top5Ch
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// The element-type-specific half of an audio element (§3.7).
///
/// Non-exhaustive: IAMF revisions keep adding element types.
#[non_exhaustive]
pub enum AudioElementConfig {
    /// §3.7.4 scalable channel layout config.
    ChannelBased {
        /// The scalable layers, lowest first.
        layers: Vec<ChannelAudioLayer>,
    },
    /// §3.7.5 ambisonics config, MONO mode.
    AmbisonicsMono {
        /// ACN channel count ((order+1)²).
        output_channel_count: u8,
        /// Mono substreams carrying the mapped channels.
        substream_count: u8,
        /// ACN slot → substream index; 255 = silent channel.
        channel_mapping: Vec<u8>,
    },
    /// §3.7.5 ambisonics config, PROJECTION mode.
    AmbisonicsProjection {
        /// ACN channel count ((order+1)²).
        output_channel_count: u8,
        /// Substreams feeding the demixing matrix.
        substream_count: u8,
        /// Of those, how many are coupled (stereo).
        coupled_substream_count: u8,
        /// (substream_count + coupled_substream_count) rows of
        /// output_channel_count Q1.15 entries, decoded-channel-major
        /// (entry `[c * output_channel_count + acn]`).
        demixing_matrix: Vec<i16>,
    },
    /// IAMF v2.0 `objects_config` (audio_element_type 2): one substream
    /// carrying `num_objects` (1 or 2) objects, positioned by a sub-mix
    /// element's position parameter.
    ObjectBased {
        /// 1 (mono substream) or 2 (coupled substream).
        num_objects: u8,
        /// `objects_config_extension_bytes`, uninterpreted.
        extension: Vec<u8>,
    },
    /// Reserved audio_element_type 3..=7: the sized config was skipped;
    /// decoders ignore such elements.
    Extension {
        /// The reserved audio_element_type.
        element_type: u8,
        /// The raw `audio_element_config` bytes.
        config: Vec<u8>,
    },
}

/// Audio element descriptor (§3.7): a set of substreams that decode into
/// one channel-based, scene-based or object-based element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioElement {
    /// Identifier mix presentations reference.
    pub audio_element_id: u32,
    /// The codec config all substreams of this element use.
    pub codec_config_id: u32,
    /// Substream ids, in decode order.
    pub substream_ids: Vec<u32>,
    /// Parameters (demixing, recon gain) declared by the element.
    pub params: Vec<ElementParam>,
    /// The element-type-specific layout config.
    pub config: AudioElementConfig,
}

impl AudioElement {
    #[allow(clippy::redundant_closure_for_method_calls)]
    /// Parses an audio element OBU payload.
    pub fn parse(r: &mut ByteReader<'_>) -> Result<Self> {
        let audio_element_id = r.read_leb128()?;
        let element_type = r.read_u8()? >> 5 & 0x07;
        let codec_config_id = r.read_leb128()?;
        let num_substreams = r.read_leb128()?;
        let substream_ids = read_bounded_vec(r, num_substreams, |r| r.read_leb128())?;

        let num_params = r.read_leb128()?;
        let params = read_bounded_vec(r, num_params, |r| {
            let param_type = r.read_leb128()?;
            Ok(match param_type {
                PARAM_TYPE_DEMIXING => {
                    let base = ParamDefinition::parse(r)?;
                    let byte = r.read_u8()?;
                    let default_demixing_mode = byte >> 5 & 0x07;
                    let default_weight_index = r.read_u8()? >> 4 & 0x0f;
                    ElementParam::Demixing {
                        base,
                        default_demixing_mode,
                        default_weight_index,
                    }
                }
                PARAM_TYPE_RECON_GAIN => ElementParam::ReconGain(ParamDefinition::parse(r)?),
                _ => {
                    // Reserved types carry param_definition_size for skipping.
                    let size = r.read_leb128()?;
                    r.skip(size as usize)?;
                    ElementParam::Unknown { param_type }
                }
            })
        })?;

        let config = match element_type {
            0 => Self::parse_channel_config(r)?,
            1 => Self::parse_ambisonics_config(r)?,
            2 => {
                // iamf-tools: an object-based element has exactly one
                // substream and no parameters.
                if num_substreams != 1 || num_params != 0 {
                    return Err(invalid(r));
                }
                Self::parse_objects_config(r)?
            }
            _ => {
                // Reserved types: audio_element_config_size, then bytes.
                let size = r.read_leb128()? as usize;
                AudioElementConfig::Extension {
                    element_type,
                    config: r.read_bytes(size)?.to_vec(),
                }
            }
        };

        Ok(AudioElement {
            audio_element_id,
            codec_config_id,
            substream_ids,
            params,
            config,
        })
    }

    fn parse_channel_config(r: &mut ByteReader<'_>) -> Result<AudioElementConfig> {
        const EXPANDED_LAYOUT: u8 = 15;
        let num_layers = r.read_u8()? >> 5 & 0x07;
        if num_layers == 0 {
            return Err(invalid(r));
        }
        let mut layers = Vec::with_capacity(usize::from(num_layers));
        for i in 0..num_layers {
            let byte = r.read_u8()?;
            let loudspeaker_layout = byte >> 4 & 0x0f;
            let output_gain_is_present = byte >> 3 & 0x01 != 0;
            let recon_gain_is_present = byte >> 2 & 0x01 != 0;
            let substream_count = r.read_u8()?;
            let coupled_substream_count = r.read_u8()?;
            let output_gain = if output_gain_is_present {
                let flags = r.read_u8()? >> 2 & 0x3f;
                Some((flags, r.read_i16_be()?))
            } else {
                None
            };
            let expanded_loudspeaker_layout = (i == 0 && loudspeaker_layout == EXPANDED_LAYOUT)
                .then(|| r.read_u8())
                .transpose()?;
            layers.push(ChannelAudioLayer {
                loudspeaker_layout,
                substream_count,
                coupled_substream_count,
                recon_gain_is_present,
                output_gain,
                expanded_loudspeaker_layout,
            });
        }
        Ok(AudioElementConfig::ChannelBased { layers })
    }

    fn parse_ambisonics_config(r: &mut ByteReader<'_>) -> Result<AudioElementConfig> {
        const MODE_MONO: u32 = 0;
        const MODE_PROJECTION: u32 = 1;
        match r.read_leb128()? {
            MODE_MONO => {
                let output_channel_count = r.read_u8()?;
                let substream_count = r.read_u8()?;
                let channel_mapping = r.read_bytes(usize::from(output_channel_count))?.to_vec();
                Ok(AudioElementConfig::AmbisonicsMono {
                    output_channel_count,
                    substream_count,
                    channel_mapping,
                })
            }
            MODE_PROJECTION => {
                let output_channel_count = r.read_u8()?;
                let substream_count = r.read_u8()?;
                let coupled_substream_count = r.read_u8()?;
                let entries = (usize::from(substream_count) + usize::from(coupled_substream_count))
                    * usize::from(output_channel_count);
                // Check the input holds the matrix before allocating it.
                let demixing_matrix = r
                    .read_bytes(entries * 2)?
                    .chunks_exact(2)
                    .map(|b| i16::from_be_bytes([b[0], b[1]]))
                    .collect();
                Ok(AudioElementConfig::AmbisonicsProjection {
                    output_channel_count,
                    substream_count,
                    coupled_substream_count,
                    demixing_matrix,
                })
            }
            _ => Err(invalid(r)),
        }
    }

    /// IAMF v2.0 objects_config, per iamf-tools
    /// `ObjectsConfig::CreateFromBuffer`: `objects_config_size` (uleb128, at
    /// least 1, covering the rest), `num_objects` (1 or 2), then
    /// `objects_config_size - 1` extension bytes.
    fn parse_objects_config(r: &mut ByteReader<'_>) -> Result<AudioElementConfig> {
        let size = r.read_leb128()? as usize;
        if size == 0 {
            return Err(invalid(r));
        }
        let num_objects = r.read_u8()?;
        if !(1..=2).contains(&num_objects) {
            return Err(invalid(r));
        }
        let extension = r.read_bytes(size - 1)?.to_vec();
        Ok(AudioElementConfig::ObjectBased {
            num_objects,
            extension,
        })
    }
}

// ---------------------------------------------------------------------------
// Mix presentation (§3.8)
// ---------------------------------------------------------------------------

/// §3.8.3 layout for loudness measurement / rendering targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// layout_type 2: ITU-R BS.2051 sound system letter (0..=9 → A..=J,
    /// then extensions: 10 = 7.1.2, 11 = 3.1.2, 12 = mono, 13 = 9.1.6,
    /// and — IAMF v2.0 — 14 = 7.1.5.4).
    LoudspeakersSsConvention {
        /// Sound system number (the [`crate::descriptors`] numbering
        /// shared with mix rendering targets).
        sound_system: u8,
    },
    /// layout_type 3.
    Binaural,
    /// Reserved layout types 0..=1.
    Reserved {
        /// The reserved layout_type value.
        layout_type: u8,
    },
}

impl Layout {
    fn parse(r: &mut ByteReader<'_>) -> Result<Self> {
        let byte = r.read_u8()?;
        Ok(match byte >> 6 & 0x03 {
            2 => Layout::LoudspeakersSsConvention {
                sound_system: byte >> 2 & 0x0f,
            },
            3 => Layout::Binaural,
            layout_type => Layout::Reserved { layout_type },
        })
    }
}

/// §3.8.4 loudness_info. Gains/loudness values are Q7.8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoudnessInfo {
    /// Bitmask of optional measurements present (§3.8.4 info_type; see
    /// the `LoudnessInfo::*` bit constants).
    pub info_type: u8,
    /// Integrated loudness of the mix for this layout, Q7.8 LKFS.
    pub integrated_loudness: i16,
    /// Digital (sample) peak, Q7.8 dBFS.
    pub digital_peak: i16,
    /// True peak, Q7.8 dBFS, when measured.
    pub true_peak: Option<i16>,
    /// (anchor_element, anchored_loudness) pairs.
    pub anchored_loudness: Vec<(u8, i16)>,
    /// `info_type_bytes`: the sized layout-extension region present when
    /// any bit of [`LoudnessInfo::ANY_LAYOUT_EXTENSION`] is set
    /// (uninterpreted; e.g. momentary loudness / loudness range).
    pub layout_extension: Vec<u8>,
}

impl LoudnessInfo {
    /// info_type bit: `true_peak` is present.
    pub const TRUE_PEAK: u8 = 0x01;
    /// info_type bit: anchored loudness entries are present.
    pub const ANCHORED_LOUDNESS: u8 = 0x02;
    /// info_type bit (IAMF v2.0 `LOUDNESS_INFO_TYPE_LIVE`): the values
    /// were measured live rather than over the whole program.
    pub const LIVE: u8 = 0x04;
    /// info_type bits that make a sized `info_type_bytes` region follow
    /// (iamf-tools `kAnyLayoutExtension`; includes [`LoudnessInfo::LIVE`]).
    pub const ANY_LAYOUT_EXTENSION: u8 = 0xfc;

    /// Whether the loudness was measured live
    /// ([`LoudnessInfo::LIVE`]).
    pub fn is_live(&self) -> bool {
        self.info_type & Self::LIVE != 0
    }

    fn parse(r: &mut ByteReader<'_>) -> Result<Self> {
        let info_type = r.read_u8()?;
        let integrated_loudness = r.read_i16_be()?;
        let digital_peak = r.read_i16_be()?;
        let true_peak = (info_type & Self::TRUE_PEAK != 0)
            .then(|| r.read_i16_be())
            .transpose()?;
        let mut anchored_loudness = Vec::new();
        if info_type & Self::ANCHORED_LOUDNESS != 0 {
            let count = r.read_u8()?;
            for _ in 0..count {
                anchored_loudness.push((r.read_u8()?, r.read_i16_be()?));
            }
        }
        let mut layout_extension = Vec::new();
        if info_type & Self::ANY_LAYOUT_EXTENSION != 0 {
            // Extension bits set: a sized extension region follows.
            let size = r.read_leb128()?;
            layout_extension = r.read_bytes(size as usize)?.to_vec();
        }
        Ok(LoudnessInfo {
            info_type,
            integrated_loudness,
            digital_peak,
            true_peak,
            anchored_loudness,
            layout_extension,
        })
    }
}

/// §3.8.2 headphones_rendering_mode (2 bits), with the IAMF v2.0 names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HeadphonesRenderingMode {
    /// 0: render to stereo loudspeakers, then to headphones.
    Stereo,
    /// 1: binaural, world-locked (IAMF v1.1 "binaural").
    BinauralWorldLocked,
    /// 2: binaural, head-locked (IAMF v2.0; reserved in v1.1).
    BinauralHeadLocked,
    /// 3: reserved.
    Reserved,
}

impl HeadphonesRenderingMode {
    /// Maps the 2-bit coded value (higher bits are ignored).
    pub fn from_u8(value: u8) -> Self {
        match value & 0x03 {
            0 => HeadphonesRenderingMode::Stereo,
            1 => HeadphonesRenderingMode::BinauralWorldLocked,
            2 => HeadphonesRenderingMode::BinauralHeadLocked,
            _ => HeadphonesRenderingMode::Reserved,
        }
    }

    /// The coded value.
    pub fn as_u8(self) -> u8 {
        match self {
            HeadphonesRenderingMode::Stereo => 0,
            HeadphonesRenderingMode::BinauralWorldLocked => 1,
            HeadphonesRenderingMode::BinauralHeadLocked => 2,
            HeadphonesRenderingMode::Reserved => 3,
        }
    }
}

/// IAMF v2.0 rendering_config `binaural_filter_profile` (2 bits).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum BinauralFilterProfile {
    /// 0: ambient (the default, and what v1.1 streams signal).
    #[default]
    Ambient,
    /// 1: direct.
    Direct,
    /// 2: reverberant.
    Reverberant,
    /// 3: reserved.
    Reserved,
}

impl BinauralFilterProfile {
    /// Maps the 2-bit coded value (higher bits are ignored).
    pub fn from_u8(value: u8) -> Self {
        match value & 0x03 {
            0 => BinauralFilterProfile::Ambient,
            1 => BinauralFilterProfile::Direct,
            2 => BinauralFilterProfile::Reverberant,
            _ => BinauralFilterProfile::Reserved,
        }
    }
}

/// IAMF v2.0 `element_gain_offset_config` (in the rendering-config
/// extension when `element_gain_offset_flag` is set): a playback-time gain
/// the user may apply to the element. Values are Q7.8 dB.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ElementGainOffsetConfig {
    /// Type 0: a fixed offset.
    Value {
        /// The offset, Q7.8 dB.
        offset: i16,
    },
    /// Type 1: a user-adjustable offset within [min, max].
    ///
    /// The parser requires `min <= default <= max`, comparing the three
    /// coded values directly, as iamf-tools does. Spec v2.0 §3.7.1.1
    /// instead defines `min` and `max` as offsets relative to `default`;
    /// this crate follows iamf-tools.
    Range {
        /// Offset applied unless the user picks another, Q7.8 dB.
        default: i16,
        /// Lowest allowed offset, Q7.8 dB (absolute, like `default`; must
        /// not exceed it).
        min: i16,
        /// Highest allowed offset, Q7.8 dB (absolute, like `default`; must
        /// not be below it).
        max: i16,
    },
    /// Reserved types 2..=255: sized payload, uninterpreted.
    Extension {
        /// The reserved `element_gain_offset_config_type`.
        config_type: u8,
        /// The raw payload.
        bytes: Vec<u8>,
    },
}

impl ElementGainOffsetConfig {
    /// Parses per iamf-tools `ElementGainOffsetConfig::CreateFromBuffer`
    /// (a range type must satisfy `min <= default <= max`).
    fn parse(r: &mut ByteReader<'_>) -> Result<Self> {
        Ok(match r.read_u8()? {
            0 => ElementGainOffsetConfig::Value {
                offset: r.read_i16_be()?,
            },
            1 => {
                let default = r.read_i16_be()?;
                let min = r.read_i16_be()?;
                let max = r.read_i16_be()?;
                if !(min..=max).contains(&default) {
                    return Err(invalid(r));
                }
                ElementGainOffsetConfig::Range { default, min, max }
            }
            config_type => {
                let size = r.read_leb128()? as usize;
                ElementGainOffsetConfig::Extension {
                    config_type,
                    bytes: r.read_bytes(size)?.to_vec(),
                }
            }
        })
    }

    /// The offset to apply by default, Q7.8 dB (`None` for reserved
    /// types).
    pub fn default_offset(&self) -> Option<i16> {
        match *self {
            ElementGainOffsetConfig::Value { offset } => Some(offset),
            ElementGainOffsetConfig::Range { default, .. } => Some(default),
            ElementGainOffsetConfig::Extension { .. } => None,
        }
    }
}

/// One audio element's entry in a sub mix (§3.8.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubMixElement {
    /// The referenced audio element.
    pub audio_element_id: u32,
    /// Human-readable labels, one per annotation language.
    pub localized_annotations: Vec<String>,
    /// §3.8.2 rendering_config headphones_rendering_mode (0..=3; see
    /// [`SubMixElement::headphones_mode`]).
    pub headphones_rendering_mode: u8,
    /// IAMF v2.0 rendering_config binaural_filter_profile.
    pub binaural_filter_profile: BinauralFilterProfile,
    /// IAMF v2.0 position parameter definitions from the rendering-config
    /// extension (object-based elements).
    pub position_params: Vec<PositionParamDefinition>,
    /// IAMF v2.0 element gain offset, when `element_gain_offset_flag` is
    /// set and the rendering-config extension carries one.
    pub element_gain_offset: Option<ElementGainOffsetConfig>,
    /// This element's mix gain into the sub mix.
    pub element_mix_gain: MixGainParam,
}

impl SubMixElement {
    /// [`SubMixElement::headphones_rendering_mode`] with its IAMF v2.0
    /// name.
    pub fn headphones_mode(&self) -> HeadphonesRenderingMode {
        HeadphonesRenderingMode::from_u8(self.headphones_rendering_mode)
    }
}

/// The parsed `rendering_config` of a sub-mix element.
struct RenderingConfig {
    headphones_rendering_mode: u8,
    binaural_filter_profile: BinauralFilterProfile,
    position_params: Vec<PositionParamDefinition>,
    element_gain_offset: Option<ElementGainOffsetConfig>,
}

impl RenderingConfig {
    /// §3.8.2 rendering_config, per iamf-tools
    /// `RenderingConfig::CreateFromBuffer`: headphones_rendering_mode (2
    /// bits), element_gain_offset_flag (1), binaural_filter_profile (2),
    /// reserved (3), rendering_config_extension_size, then the extension.
    ///
    /// The extension holds `num_params` position parameter definitions and,
    /// when flagged, an element gain offset config; bytes after those are
    /// future extensions and skipped. The extension is parsed only within
    /// its `rendering_config_extension_size` window. As in iamf-tools, an
    /// extension that fails to parse there (an unknown parameter type, or
    /// content that would overrun the window) is treated as opaque and
    /// skipped whole. In IAMF v1.1 the bits after the mode were reserved
    /// and parsers SHOULD ignore the extension bytes, which this matches.
    fn parse(r: &mut ByteReader<'_>) -> Result<Self> {
        let byte = r.read_u8()?;
        let headphones_rendering_mode = byte >> 6 & 0x03;
        let element_gain_offset_flag = byte >> 5 & 0x01 != 0;
        let binaural_filter_profile = BinauralFilterProfile::from_u8(byte >> 3);
        let extension_size = r.read_leb128()? as usize;
        let mut config = RenderingConfig {
            headphones_rendering_mode,
            binaural_filter_profile,
            position_params: Vec::new(),
            element_gain_offset: None,
        };
        if extension_size == 0 {
            return Ok(config);
        }
        // Parse only inside the extension window. Probing the rest of the
        // OBU made each element rescan to its end (quadratic time), and
        // let reserved v1.1 extension bytes that happened to parse as
        // v2.0 syntax run past the window and reject the mix.
        let mut window = ByteReader::new(r.read_bytes(extension_size)?);
        if let Ok((params, gain_offset)) =
            Self::parse_extension(&mut window, element_gain_offset_flag)
        {
            config.position_params = params;
            config.element_gain_offset = gain_offset;
        }
        Ok(config)
    }

    #[allow(clippy::type_complexity)]
    fn parse_extension(
        r: &mut ByteReader<'_>,
        element_gain_offset_flag: bool,
    ) -> Result<(
        Vec<PositionParamDefinition>,
        Option<ElementGainOffsetConfig>,
    )> {
        let num_params = r.read_leb128()?;
        let params = read_bounded_vec(r, num_params, |r| {
            let param_definition_type = r.read_leb128()?;
            match PositionParamType::from_param_definition_type(param_definition_type) {
                Some(ty) => PositionParamDefinition::parse(r, ty),
                // iamf-tools skips the sized definition, then reports the
                // type as unsupported, which makes the whole extension
                // opaque.
                None => Err(invalid(r)),
            }
        })?;
        let gain_offset = element_gain_offset_flag
            .then(|| ElementGainOffsetConfig::parse(r))
            .transpose()?;
        Ok((params, gain_offset))
    }
}

/// One sub mix of a mix presentation (§3.8.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubMix {
    /// The audio elements summed into this sub mix. Empty only in
    /// non-conformant streams (§3.7: num_audio_elements SHALL NOT be 0);
    /// such a sub mix must not be rendered.
    pub elements: Vec<SubMixElement>,
    /// Gain applied to the summed output.
    pub output_mix_gain: MixGainParam,
    /// Layouts the mix was authored/measured for, with loudness for each.
    pub layouts: Vec<(Layout, LoudnessInfo)>,
}

/// IAMF v2.0 `mix_presentation_optional_fields`, present when the mix
/// presentation OBU header sets `optional_fields_flag`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct MixPresentationOptionalFields {
    /// Preferred loudspeaker renderer: 0 = none; 1..=255 reserved.
    pub preferred_loudspeaker_renderer: u8,
    /// Preferred binaural renderer: 0 = none; 1..=255 reserved.
    pub preferred_binaural_renderer: u8,
    /// Remaining `optional_fields_size - 2` bytes, uninterpreted.
    pub extension: Vec<u8>,
}

impl MixPresentationOptionalFields {
    /// Parses per iamf-tools `MixPresentationOptionalFields::CreateFromBuffer`
    /// (`optional_fields_size` must be at least 2).
    fn parse(r: &mut ByteReader<'_>) -> Result<Self> {
        let size = r.read_leb128()? as usize;
        if size < 2 {
            return Err(invalid(r));
        }
        Ok(MixPresentationOptionalFields {
            preferred_loudspeaker_renderer: r.read_u8()?,
            preferred_binaural_renderer: r.read_u8()?,
            extension: r.read_bytes(size - 2)?.to_vec(),
        })
    }
}

/// Mix presentation descriptor (§3.8): a renderable presentation of one
/// or more audio elements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MixPresentation {
    /// Identifier used for selection.
    pub mix_presentation_id: u32,
    /// BCP-47-ish language tags, one per label.
    pub annotation_languages: Vec<String>,
    /// Human-readable presentation labels, one per language.
    pub localized_annotations: Vec<String>,
    /// The sub mixes (IAMF v1.1 requires exactly one).
    pub sub_mixes: Vec<SubMix>,
    /// §3.7.5 mix presentation tags (name, value), when present, in
    /// bitstream order. As in iamf-tools' permissive decoding, restricted
    /// tags that violate their rule are dropped: `content_language` must
    /// be a three-character ISO 639-2 code and, like `content_type`, may
    /// occur at most once (the first valid instance is kept).
    pub tags: Vec<(String, String)>,
    /// IAMF v2.0 optional fields, when the OBU header flags them.
    pub optional_fields: Option<MixPresentationOptionalFields>,
}

impl MixPresentation {
    /// Parses a mix presentation OBU payload whose header did not set
    /// `optional_fields_flag` (every IAMF v1.x stream).
    pub fn parse(r: &mut ByteReader<'_>) -> Result<Self> {
        Self::parse_with_optional_fields(r, false)
    }

    /// Parses a mix presentation OBU payload; `optional_fields_flag` is the
    /// OBU header's type-specific flag
    /// ([`crate::ObuHeader::optional_fields_flag`]). Per iamf-tools, a
    /// flagged OBU must carry the tags block and then the optional fields.
    #[allow(clippy::redundant_closure_for_method_calls)]
    pub fn parse_with_optional_fields(
        r: &mut ByteReader<'_>,
        optional_fields_flag: bool,
    ) -> Result<Self> {
        let mix_presentation_id = r.read_leb128()?;
        let count_label = r.read_leb128()?;
        let annotation_languages = read_bounded_vec(r, count_label, |r| r.read_string())?;
        let localized_annotations = read_bounded_vec(r, count_label, |r| r.read_string())?;

        let num_sub_mixes = r.read_leb128()?;
        let sub_mixes = read_bounded_vec(r, num_sub_mixes, |r| {
            // §3.7: num_audio_elements SHALL NOT be 0. The sub mix is still
            // well-formed syntax, so it parses (with no elements) and the
            // decoder refuses to render it: one bad mix presentation must
            // not invalidate the others (iamf-tools drops such a mix and
            // never renders a sub mix without audio elements).
            let num_elements = r.read_leb128()?;
            let elements = read_bounded_vec(r, num_elements, |r| {
                let audio_element_id = r.read_leb128()?;
                let localized_annotations = read_bounded_vec(r, count_label, |r| r.read_string())?;
                let rendering = RenderingConfig::parse(r)?;
                let element_mix_gain = MixGainParam::parse(r)?;
                Ok(SubMixElement {
                    audio_element_id,
                    localized_annotations,
                    headphones_rendering_mode: rendering.headphones_rendering_mode,
                    binaural_filter_profile: rendering.binaural_filter_profile,
                    position_params: rendering.position_params,
                    element_gain_offset: rendering.element_gain_offset,
                    element_mix_gain,
                })
            })?;
            let output_mix_gain = MixGainParam::parse(r)?;
            let num_layouts = r.read_leb128()?;
            let layouts = read_bounded_vec(r, num_layouts, |r| {
                let layout = Layout::parse(r)?;
                let loudness = LoudnessInfo::parse(r)?;
                Ok((layout, loudness))
            })?;
            Ok(SubMix {
                elements,
                output_mix_gain,
                layouts,
            })
        })?;

        // Optional trailing tags (added in v1.1): num_tags then name/value
        // C-string pairs.
        let mut tags = Vec::new();
        if !r.is_empty() {
            let num_tags = r.read_u8()?;
            let mut restricted_counts = [0usize; TAG_RESTRICTIONS.len()];
            for _ in 0..num_tags {
                let name = r.read_string()?;
                let (value, value_len) = r.read_string_with_len()?;
                if keep_tag(&name, value_len, &mut restricted_counts) {
                    tags.push((name, value));
                }
            }
        } else if optional_fields_flag {
            // v2.0: flagged optional fields follow the (mandatory) tags.
            return Err(invalid(r));
        }
        let optional_fields = optional_fields_flag
            .then(|| MixPresentationOptionalFields::parse(r))
            .transpose()?;

        Ok(MixPresentation {
            mix_presentation_id,
            annotation_languages,
            localized_annotations,
            sub_mixes,
            tags,
            optional_fields,
        })
    }
}

/// One restricted mix presentation tag name (iamf-tools `TagRestriction`).
struct TagRestriction {
    name: &'static str,
    max_count: usize,
    /// Whether the value must be an ISO 639-2 code.
    iso_639_2: bool,
}

/// Tags are freeform and may repeat, except these (iamf-tools
/// `GetTagRestrictions`, iamf/obu/mix_presentation.cc). §3.7.5: at most
/// one `content_language`, whose value SHALL conform to ISO 639-2
/// (iamf-tools accepts any three-character value); `content_type` is also
/// limited to one instance (iamf-tools 25a3dbb3b).
const TAG_RESTRICTIONS: [TagRestriction; 2] = [
    TagRestriction {
        name: "content_language",
        max_count: 1,
        iso_639_2: true,
    },
    TagRestriction {
        name: "content_type",
        max_count: 1,
        iso_639_2: false,
    },
];

/// iamf-tools `ValidateTag` on decode: whether to keep a tag, counting
/// kept instances of restricted names in `counts` (indexed like
/// [`TAG_RESTRICTIONS`]). `value_len` is the raw byte length of the value,
/// as iamf-tools checks it (lossy UTF-8 conversion can change the length).
/// A value that fails validation is dropped without
/// counting, so a later valid instance can still be kept; beyond
/// `max_count`, instances are dropped (§3.7.5: "parsers SHOULD use the
/// tag_value corresponding to the first instance").
fn keep_tag(name: &str, value_len: usize, counts: &mut [usize; TAG_RESTRICTIONS.len()]) -> bool {
    let Some(index) = TAG_RESTRICTIONS.iter().position(|t| t.name == name) else {
        return true;
    };
    let restriction = &TAG_RESTRICTIONS[index];
    if restriction.iso_639_2 && value_len != 3 {
        return false;
    }
    counts[index] += 1;
    counts[index] <= restriction.max_count
}

/// Upper bound on the items [`read_bounded_vec`] reserves up front.
const MAX_PREALLOCATED_ITEMS: usize = 64;

/// How many of `count` list items to reserve before parsing them.
fn preallocation(count: usize) -> usize {
    count.min(MAX_PREALLOCATED_ITEMS)
}

/// Reads `count` items, guarding against absurd counts from a hostile
/// bitstream: each item must consume at least one byte, so `count` can never
/// legitimately exceed the bytes remaining.
///
/// Callers pass `|r| r.read_x()` closures rather than method paths: the
/// paths pin a single reader lifetime and fail higher-ranked inference.
fn read_bounded_vec<T>(
    r: &mut ByteReader<'_>,
    count: u32,
    mut read: impl FnMut(&mut ByteReader<'_>) -> Result<T>,
) -> Result<Vec<T>> {
    let count = count as usize;
    if count > r.remaining() {
        return Err(Error::UnexpectedEof {
            offset: r.position(),
        });
    }
    // `count` is bounded by the input bytes, but each item can be far
    // larger in memory than the one byte it may occupy on the wire, and
    // nested lists pre-allocate at once. Cap the reservation so memory
    // only grows as items actually parse.
    let mut items = Vec::with_capacity(preallocation(count));
    for _ in 0..count {
        items.push(read(r)?);
    }
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_header() {
        let mut r = ByteReader::new(b"iamf\x00\x01");
        let sh = SequenceHeader::parse(&mut r).unwrap();
        assert_eq!(sh.primary_profile, 0);
        assert_eq!(sh.additional_profile, 1);

        let mut r = ByteReader::new(b"OggS\x00\x01");
        assert!(SequenceHeader::parse(&mut r).is_err());
    }

    #[test]
    fn codec_config_opus() {
        let mut payload = vec![0x00]; // codec_config_id = 0
        payload.extend(b"Opus");
        payload.extend([0xc0, 0x07]); // num_samples_per_frame = 960
        payload.extend((-4i16).to_be_bytes()); // audio_roll_distance
        payload.extend([1, 2]); // version, output_channel_count
        payload.extend(312u16.to_be_bytes()); // pre_skip
        payload.extend(48000u32.to_be_bytes()); // input_sample_rate
        payload.extend(0i16.to_be_bytes()); // output_gain
        payload.push(0); // mapping_family

        let cc = CodecConfig::parse(&mut ByteReader::new(&payload)).unwrap();
        assert_eq!(cc.codec_id, CodecId::Opus);
        assert_eq!(cc.num_samples_per_frame, 960);
        assert_eq!(cc.audio_roll_distance, -4);
        assert_eq!(
            cc.decoder_config,
            DecoderConfig::Opus {
                version: 1,
                output_channel_count: 2,
                pre_skip: 312,
                input_sample_rate: 48000,
                output_gain: 0,
                mapping_family: 0,
            }
        );
    }

    #[test]
    fn codec_config_lpcm() {
        let mut payload = vec![0x01]; // codec_config_id
        payload.extend(b"ipcm");
        payload.push(0x40); // num_samples_per_frame = 64
        payload.extend(0i16.to_be_bytes());
        payload.push(0x01); // sample_format_flags: little endian
        payload.push(16); // sample_size
        payload.extend(44100u32.to_be_bytes());

        let cc = CodecConfig::parse(&mut ByteReader::new(&payload)).unwrap();
        assert_eq!(
            cc.decoder_config,
            DecoderConfig::Lpcm {
                little_endian: true,
                sample_size: 16,
                sample_rate: 44100
            }
        );
    }

    #[test]
    fn codec_config_flac_streaminfo() {
        let mut payload = vec![0x02];
        payload.extend(b"fLaC");
        payload.push(0x10); // num_samples_per_frame = 16
        payload.extend(0i16.to_be_bytes());
        // Metadata block header: last=1, type=0 (STREAMINFO), length=34.
        payload.extend(0x8000_0022u32.to_be_bytes());
        payload.extend([0u8; 10]); // block/frame sizes
        // sample_rate=48000 (20 bits), channels-1=1 (3 bits), bps-1=15 (5).
        let packed: u32 = (48000 << 12) | (1 << 9) | (15 << 4);
        payload.extend(packed.to_be_bytes());
        payload.extend([0u8; 20]); // rest of STREAMINFO, unread

        let cc = CodecConfig::parse(&mut ByteReader::new(&payload)).unwrap();
        let DecoderConfig::Flac {
            sample_rate,
            bits_per_sample,
            streaminfo,
        } = &cc.decoder_config
        else {
            panic!("expected flac");
        };
        assert_eq!(*sample_rate, 48000);
        assert_eq!(*bits_per_sample, 16);
        assert_eq!(streaminfo.len(), 34);
    }

    /// Stereo channel-based element: 1 layer, no params.
    #[test]
    fn audio_element_channel_based() {
        let mut payload = vec![0x0a]; // audio_element_id = 10
        payload.push(0x00); // type=0 (channel based)
        payload.push(0x00); // codec_config_id
        payload.push(0x01); // num_substreams
        payload.push(0x11); // substream id 17
        payload.push(0x00); // num_parameters
        payload.push(1 << 5); // num_layers = 1
        payload.push(0x01 << 4); // loudspeaker_layout=1 (stereo), no flags
        payload.push(1); // substream_count
        payload.push(1); // coupled_substream_count

        let ae = AudioElement::parse(&mut ByteReader::new(&payload)).unwrap();
        assert_eq!(ae.audio_element_id, 10);
        assert_eq!(ae.substream_ids, vec![17]);
        let AudioElementConfig::ChannelBased { layers } = &ae.config else {
            panic!("expected channel based");
        };
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].loudspeaker_layout, 1);
        assert_eq!(layers[0].coupled_substream_count, 1);
    }

    /// Element with a demixing parameter (mode 0, constant subblocks).
    #[test]
    fn audio_element_with_demixing_param() {
        let mut payload = vec![0x0b, 0x00, 0x00];
        payload.push(0x02); // num_substreams = 2
        payload.extend([0x00, 0x01]);
        payload.push(0x01); // num_parameters = 1
        payload.push(0x01); // param_definition_type = demixing
        payload.push(0x07); // parameter_id
        payload.extend([0x80, 0xf7, 0x02]); // parameter_rate = 48000
        payload.push(0x00); // mode = 0
        payload.extend([0xc0, 0x07]); // duration = 960
        payload.extend([0xc0, 0x07]); // constant_subblock_duration = 960
        payload.push(0x02 << 5); // dmixp_mode = 2
        payload.push(0x03 << 4); // default_w = 3
        payload.push(1 << 5); // num_layers = 1
        payload.push(0x02 << 4); // layout 2 (5.1)
        payload.push(4);
        payload.push(2);

        let ae = AudioElement::parse(&mut ByteReader::new(&payload)).unwrap();
        assert_eq!(ae.params.len(), 1);
        let ElementParam::Demixing {
            base,
            default_demixing_mode,
            default_weight_index,
        } = &ae.params[0]
        else {
            panic!("expected demixing param");
        };
        assert_eq!(base.parameter_rate, 48000);
        assert_eq!(base.duration, 960);
        assert_eq!(*default_demixing_mode, 2);
        assert_eq!(*default_weight_index, 3);
    }

    #[test]
    fn audio_element_ambisonics_mono() {
        let mut payload = vec![0x0c];
        payload.push(0x01 << 5); // type=1 (scene based)
        payload.push(0x00); // codec_config_id
        payload.push(0x04); // num_substreams
        payload.extend([0x00, 0x01, 0x02, 0x03]);
        payload.push(0x00); // num_parameters
        payload.push(0x00); // ambisonics_mode = mono
        payload.push(4); // output_channel_count (FOA)
        payload.push(4); // substream_count
        payload.extend([0, 1, 2, 3]); // channel_mapping

        let ae = AudioElement::parse(&mut ByteReader::new(&payload)).unwrap();
        assert_eq!(
            ae.config,
            AudioElementConfig::AmbisonicsMono {
                output_channel_count: 4,
                substream_count: 4,
                channel_mapping: vec![0, 1, 2, 3],
            }
        );
    }

    /// Minimal mix presentation: one label, one sub mix, one element, one
    /// stereo layout with basic loudness.
    #[test]
    fn mix_presentation_minimal() {
        let mut payload = vec![0x2a]; // mix_presentation_id = 42
        payload.push(0x01); // count_label
        payload.extend(b"en-us\0");
        payload.extend(b"Default\0");
        payload.push(0x01); // num_sub_mixes
        payload.push(0x01); // num_audio_elements
        payload.push(0x0a); // audio_element_id = 10
        payload.extend(b"Main\0"); // localized element annotation
        payload.push(0x00); // headphones_rendering_mode=0, reserved
        payload.push(0x00); // rendering_config_extension_size = 0
        // element_mix_gain: id, rate, mode=1 (no timing), default gain.
        payload.push(0x00);
        payload.extend([0x80, 0xf7, 0x02]);
        payload.push(0x80);
        payload.extend(0i16.to_be_bytes());
        // output_mix_gain, same shape.
        payload.push(0x01);
        payload.extend([0x80, 0xf7, 0x02]);
        payload.push(0x80);
        payload.extend((-256i16).to_be_bytes()); // -1 dB in Q7.8
        payload.push(0x01); // num_layouts
        payload.push(0x80); // type=2 (ss convention), sound system A (0)
        payload.push(0x00); // info_type = 0
        payload.extend((-4096i16).to_be_bytes()); // integrated loudness -16 LKFS
        payload.extend((-256i16).to_be_bytes()); // digital peak

        let mp = MixPresentation::parse(&mut ByteReader::new(&payload)).unwrap();
        assert_eq!(mp.mix_presentation_id, 42);
        assert_eq!(mp.annotation_languages, vec!["en-us"]);
        assert_eq!(mp.localized_annotations, vec!["Default"]);
        assert_eq!(mp.sub_mixes.len(), 1);
        let sub = &mp.sub_mixes[0];
        assert_eq!(sub.elements[0].audio_element_id, 10);
        assert_eq!(sub.elements[0].localized_annotations, vec!["Main"]);
        assert_eq!(sub.output_mix_gain.default_mix_gain, -256);
        assert_eq!(
            sub.layouts[0].0,
            Layout::LoudspeakersSsConvention { sound_system: 0 }
        );
        assert_eq!(sub.layouts[0].1.integrated_loudness, -4096);
        assert_eq!(mp.tags, []);
    }

    /// Payload of a minimal unlabeled mix presentation (id 42, one sub mix
    /// holding element 10, one stereo layout) whose element carries
    /// `rendering_config`. Ends after the sub mixes (no tags).
    fn minimal_mix_payload(rendering_config: &[u8]) -> Vec<u8> {
        let mut payload = vec![0x2a, 0x00, 0x01]; // id 42, no labels, 1 sub mix
        payload.extend([0x01, 0x0a]); // num_audio_elements, element 10
        payload.extend_from_slice(rendering_config);
        for parameter_id in [0x00, 0x01] {
            // element_mix_gain then output_mix_gain: mode 1, 0 dB.
            payload.push(parameter_id);
            payload.extend([0x80, 0xf7, 0x02, 0x80, 0x00, 0x00]);
        }
        payload.extend([0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00]); // stereo layout
        payload
    }

    /// Minimal unlabeled mix presentation followed by `tags` (raw bytes,
    /// NUL-terminated on the wire).
    fn mix_with_raw_tags(tags: &[(&[u8], &[u8])]) -> MixPresentation {
        let mut payload = minimal_mix_payload(&[0x00, 0x00]);
        payload.push(u8::try_from(tags.len()).unwrap());
        for (name, value) in tags {
            for s in [name, value] {
                payload.extend_from_slice(s);
                payload.push(0);
            }
        }
        let mp = MixPresentation::parse(&mut ByteReader::new(&payload)).unwrap();
        assert_eq!(mp.sub_mixes[0].elements[0].audio_element_id, 10);
        mp
    }

    /// [`mix_with_raw_tags`] for UTF-8 tags.
    fn mix_with_tags(tags: &[(&str, &str)]) -> MixPresentation {
        let raw: Vec<(&[u8], &[u8])> = tags
            .iter()
            .map(|(n, v)| (n.as_bytes(), v.as_bytes()))
            .collect();
        mix_with_raw_tags(&raw)
    }

    fn tag_pairs(mp: &MixPresentation) -> Vec<(&str, &str)> {
        mp.tags
            .iter()
            .map(|(n, v)| (n.as_str(), v.as_str()))
            .collect()
    }

    #[test]
    fn mix_presentation_tags_unrestricted_kept_in_order() {
        let tags = [
            ("artist", "a"),
            ("artist", "b"),
            ("content_language", "eng"),
        ];
        assert_eq!(tag_pairs(&mix_with_tags(&tags)), tags);
    }

    /// §3.7.5: at most one content_language; the first instance wins.
    #[test]
    fn mix_presentation_duplicate_content_language_ignored() {
        let mp = mix_with_tags(&[("content_language", "eng"), ("content_language", "kor")]);
        assert_eq!(tag_pairs(&mp), [("content_language", "eng")]);
    }

    /// content_language must be an ISO 639-2 (three-character) code. An
    /// invalid value is dropped without using up the single slot.
    #[test]
    fn mix_presentation_non_iso_639_2_content_language_ignored() {
        let mp = mix_with_tags(&[
            ("content_language", "en-us"),
            ("content_language", ""),
            ("content_language", "fra"),
            ("content_language", "deu"),
        ]);
        assert_eq!(tag_pairs(&mp), [("content_language", "fra")]);
    }

    /// The ISO 639-2 length check uses the raw byte length, as iamf-tools
    /// does, not the length after lossy UTF-8 conversion.
    #[test]
    fn mix_presentation_content_language_checks_raw_bytes() {
        // 1 raw byte, but 3 bytes (U+FFFD) after lossy conversion: dropped.
        let mp = mix_with_raw_tags(&[(b"content_language", b"\xff")]);
        assert_eq!(mp.tags, []);
        // 3 raw bytes, but 5 bytes after lossy conversion: kept.
        let mp = mix_with_raw_tags(&[(b"content_language", b"\xe9ng")]);
        assert_eq!(tag_pairs(&mp), [("content_language", "\u{fffd}ng")]);
    }

    /// iamf-tools 25a3dbb3b: content_type may occur at most once.
    #[test]
    fn mix_presentation_duplicate_content_type_ignored() {
        let mp = mix_with_tags(&[
            ("content_type", "music"),
            ("other", "x"),
            ("content_type", "dialogue"),
        ]);
        assert_eq!(tag_pairs(&mp), [("content_type", "music"), ("other", "x")]);
    }

    /// num_audio_elements == 0 violates §3.7 but is well-formed syntax: it
    /// parses to an empty sub mix, which decoders refuse to render.
    #[test]
    fn mix_presentation_empty_sub_mix_parses() {
        let mut payload = vec![0x07]; // mix_presentation_id = 7
        payload.push(0x00); // count_label
        payload.push(0x01); // num_sub_mixes
        payload.push(0x00); // num_audio_elements = 0
        // output_mix_gain: id, rate, mode=1, default gain.
        payload.push(0x01);
        payload.extend([0x80, 0xf7, 0x02]);
        payload.push(0x80);
        payload.extend(0i16.to_be_bytes());
        payload.push(0x01); // num_layouts
        payload.push(0x80); // ss convention, sound system A
        payload.push(0x00); // info_type
        payload.extend(0i16.to_be_bytes());
        payload.extend(0i16.to_be_bytes());

        let mp = MixPresentation::parse(&mut ByteReader::new(&payload)).unwrap();
        assert_eq!(mp.mix_presentation_id, 7);
        assert_eq!(mp.sub_mixes.len(), 1);
        assert_eq!(mp.sub_mixes[0].elements, []);
        assert_eq!(mp.sub_mixes[0].layouts.len(), 1);
    }

    #[test]
    fn hostile_count_rejected() {
        // num_substreams = 0xFFFFFFF far exceeds remaining bytes.
        let payload = [0x0a, 0x00, 0x00, 0xff, 0xff, 0xff, 0x7f];
        assert!(matches!(
            AudioElement::parse(&mut ByteReader::new(&payload)),
            Err(Error::UnexpectedEof { .. })
        ));
    }

    #[test]
    fn profile_version_mapping() {
        for value in 0..=u8::MAX {
            assert_eq!(ProfileVersion::from_u8(value).as_u8(), value);
        }
        assert_eq!(ProfileVersion::from_u8(2), ProfileVersion::BaseEnhanced);
        assert_eq!(ProfileVersion::from_u8(3), ProfileVersion::BaseAdvanced);
        assert_eq!(ProfileVersion::from_u8(4), ProfileVersion::Advanced1);
        assert_eq!(ProfileVersion::from_u8(5), ProfileVersion::Advanced2);
        assert_eq!(ProfileVersion::from_u8(6), ProfileVersion::Reserved(6));
        assert_eq!(ProfileVersion::from_u8(255), ProfileVersion::Reserved(255));

        let sh = SequenceHeader::parse(&mut ByteReader::new(b"iamf\x03\x05")).unwrap();
        assert_eq!(sh.primary(), ProfileVersion::BaseAdvanced);
        assert_eq!(sh.additional(), ProfileVersion::Advanced2);
    }

    #[test]
    fn expanded_layout_mapping() {
        for value in 0..=19 {
            let layout = ExpandedLayout::from_u8(value).unwrap();
            assert_eq!(layout.is_v2(), value >= 13, "layout {value}");
        }
        for value in [20, 21, 255] {
            assert_eq!(ExpandedLayout::from_u8(value), None);
        }
        let expected = [
            (0, ExpandedLayout::Lfe, 1),
            (8, ExpandedLayout::Ch9_1_6, 16),
            (12, ExpandedLayout::Top6Ch, 6),
            (13, ExpandedLayout::Ch10_2_9_3, 24),
            (14, ExpandedLayout::LfePair, 2),
            (15, ExpandedLayout::Bottom3Ch, 3),
            (16, ExpandedLayout::Ch7_1_5_4, 17),
            (17, ExpandedLayout::Bottom4Ch, 4),
            (18, ExpandedLayout::Top1Ch, 1),
            (19, ExpandedLayout::Top5Ch, 5),
        ];
        for (value, layout, channels) in expected {
            assert_eq!(ExpandedLayout::from_u8(value), Some(layout));
            assert_eq!(layout.channel_count(), channels, "{layout:?}");
        }
    }

    #[test]
    fn headphones_mode_and_binaural_profile_mapping() {
        let modes = [
            HeadphonesRenderingMode::Stereo,
            HeadphonesRenderingMode::BinauralWorldLocked,
            HeadphonesRenderingMode::BinauralHeadLocked,
            HeadphonesRenderingMode::Reserved,
        ];
        for (value, mode) in (0u8..).zip(modes) {
            assert_eq!(HeadphonesRenderingMode::from_u8(value), mode);
            assert_eq!(mode.as_u8(), value);
            // Only the low 2 bits are coded.
            assert_eq!(HeadphonesRenderingMode::from_u8(value | 0xfc), mode);
        }
        let profiles = [
            BinauralFilterProfile::Ambient,
            BinauralFilterProfile::Direct,
            BinauralFilterProfile::Reverberant,
            BinauralFilterProfile::Reserved,
        ];
        for (value, profile) in (0u8..).zip(profiles) {
            assert_eq!(BinauralFilterProfile::from_u8(value), profile);
            assert_eq!(BinauralFilterProfile::from_u8(value | 0xfc), profile);
        }
    }

    /// Parses an object-based audio element (type 2) with one substream, no
    /// parameters, and the given `objects_config` bytes. On success the
    /// payload must be consumed exactly.
    fn object_element(objects_config: &[u8]) -> Result<AudioElementConfig> {
        let mut payload = vec![0x0d, 2 << 5, 0x00]; // id 13, type 2, codec config 0
        payload.extend([0x01, 0x05]); // num_substreams = 1, substream id 5
        payload.push(0x00); // num_parameters
        payload.extend_from_slice(objects_config);
        let mut r = ByteReader::new(&payload);
        let ae = AudioElement::parse(&mut r)?;
        assert!(r.is_empty(), "trailing bytes");
        Ok(ae.config)
    }

    #[test]
    fn audio_element_objects_config() {
        // objects_config_size 1: num_objects only.
        assert_eq!(
            object_element(&[0x01, 0x01]).unwrap(),
            AudioElementConfig::ObjectBased {
                num_objects: 1,
                extension: vec![],
            }
        );
        // objects_config_size 3: num_objects plus two extension bytes.
        assert_eq!(
            object_element(&[0x03, 0x02, 0xaa, 0xbb]).unwrap(),
            AudioElementConfig::ObjectBased {
                num_objects: 2,
                extension: vec![0xaa, 0xbb],
            }
        );
    }

    /// objects_config_size is a leb128 (spec v2.0 §3.6.4, iamf-tools).
    #[test]
    fn audio_element_objects_config_size_is_leb128() {
        // 130 = 0x82 0x01: num_objects plus 129 extension bytes.
        let mut config = vec![0x82, 0x01, 0x01];
        config.extend([0x5a; 129]);
        assert_eq!(
            object_element(&config).unwrap(),
            AudioElementConfig::ObjectBased {
                num_objects: 1,
                extension: vec![0x5a; 129],
            }
        );
        // Non-minimal 3-byte encoding of 2.
        assert_eq!(
            object_element(&[0x82, 0x80, 0x00, 0x02, 0x7f]).unwrap(),
            AudioElementConfig::ObjectBased {
                num_objects: 2,
                extension: vec![0x7f],
            }
        );
        // A huge size fails on the missing bytes, without allocating.
        assert!(matches!(
            object_element(&[0xff, 0xff, 0xff, 0xff, 0x0f, 0x01]),
            Err(Error::UnexpectedEof { .. })
        ));
    }

    #[test]
    fn audio_element_objects_config_malformed() {
        // objects_config_size 0 cannot hold num_objects.
        assert!(matches!(
            object_element(&[0x00, 0x01]),
            Err(Error::InvalidDescriptor { .. })
        ));
        // num_objects must be 1 or 2.
        for num_objects in [0, 3, 255] {
            assert!(matches!(
                object_element(&[0x01, num_objects]),
                Err(Error::InvalidDescriptor { .. })
            ));
        }
        // Truncated extension bytes and truncated size.
        assert!(matches!(
            object_element(&[0x03, 0x01, 0xaa]),
            Err(Error::UnexpectedEof { .. })
        ));
        assert!(matches!(
            object_element(&[0x80]),
            Err(Error::UnexpectedEof { .. })
        ));
    }

    /// iamf-tools: object-based elements need exactly one substream and no
    /// parameters.
    #[test]
    fn audio_element_objects_substream_and_param_counts() {
        let objects_config = [0x01, 0x01];
        // num_substreams 0 and 2.
        for substreams in [&[0x00][..], &[0x02, 0x05, 0x06]] {
            let mut payload = vec![0x0d, 2 << 5, 0x00];
            payload.extend_from_slice(substreams);
            payload.push(0x00); // num_parameters
            payload.extend(objects_config);
            assert!(matches!(
                AudioElement::parse(&mut ByteReader::new(&payload)),
                Err(Error::InvalidDescriptor { .. })
            ));
        }
        // One (reserved type 9, empty) parameter.
        let mut payload = vec![0x0d, 2 << 5, 0x00, 0x01, 0x05];
        payload.extend([0x01, 0x09, 0x00]); // num_parameters, type, size
        payload.extend(objects_config);
        assert!(matches!(
            AudioElement::parse(&mut ByteReader::new(&payload)),
            Err(Error::InvalidDescriptor { .. })
        ));
    }

    /// Reserved audio_element_type 3..=7: audio_element_config_size, then
    /// opaque bytes.
    #[test]
    fn audio_element_reserved_type_is_opaque() {
        let mut payload = vec![0x0e, 3 << 5, 0x00, 0x01, 0x05, 0x00];
        payload.extend([0x02, 0xaa, 0xbb]);
        let mut r = ByteReader::new(&payload);
        let ae = AudioElement::parse(&mut r).unwrap();
        assert!(r.is_empty());
        assert_eq!(
            ae.config,
            AudioElementConfig::Extension {
                element_type: 3,
                config: vec![0xaa, 0xbb],
            }
        );

        // Type 7, no substreams, empty config.
        let payload = [0x0e, 7 << 5, 0x00, 0x00, 0x00, 0x00];
        let ae = AudioElement::parse(&mut ByteReader::new(&payload)).unwrap();
        assert_eq!(
            ae.config,
            AudioElementConfig::Extension {
                element_type: 7,
                config: vec![],
            }
        );

        // Config shorter than its size.
        let payload = [0x0e, 3 << 5, 0x00, 0x00, 0x00, 0x03, 0xaa];
        assert!(matches!(
            AudioElement::parse(&mut ByteReader::new(&payload)),
            Err(Error::UnexpectedEof { .. })
        ));
    }

    /// Parses [`minimal_mix_payload`] with `rendering_config` and returns
    /// its only sub-mix element.
    fn element_with_rendering_config(rendering_config: &[u8]) -> Result<SubMixElement> {
        let payload = minimal_mix_payload(rendering_config);
        let mut mp = MixPresentation::parse(&mut ByteReader::new(&payload))?;
        assert!(mp.tags.is_empty(), "rendering config misparsed");
        Ok(mp.sub_mixes.remove(0).elements.remove(0))
    }

    /// Polar position parameter definition (type 3): parameter_id 7, rate
    /// 48000, mode 1, default azimuth 30, elevation 0, distance 1.
    const POLAR_PARAM: [u8; 9] = [0x03, 0x07, 0x80, 0xf7, 0x02, 0x80, 0x0f, 0x00, 0x01];

    #[test]
    fn rendering_config_v1_shape() {
        let e = element_with_rendering_config(&[0x40, 0x00]).unwrap();
        assert_eq!(
            e.headphones_mode(),
            HeadphonesRenderingMode::BinauralWorldLocked
        );
        assert_eq!(e.binaural_filter_profile, BinauralFilterProfile::Ambient);
        assert_eq!(e.position_params, []);
        assert_eq!(e.element_gain_offset, None);
        // element_gain_offset_flag with an empty extension: no offset.
        let e = element_with_rendering_config(&[0x20, 0x00]).unwrap();
        assert_eq!(e.element_gain_offset, None);
    }

    #[test]
    fn rendering_config_position_param() {
        // Head-locked, direct profile, extension: num_params 1, polar param.
        let mut config = vec![(2 << 6) | (1 << 3), 0x0a, 0x01];
        config.extend(POLAR_PARAM);
        let e = element_with_rendering_config(&config).unwrap();
        assert_eq!(
            e.headphones_mode(),
            HeadphonesRenderingMode::BinauralHeadLocked
        );
        assert_eq!(e.binaural_filter_profile, BinauralFilterProfile::Direct);
        assert_eq!(e.position_params.len(), 1);
        let param = &e.position_params[0];
        assert_eq!(param.param_type, PositionParamType::Polar);
        assert_eq!(param.base.parameter_id, 7);
        assert_eq!(
            param.defaults,
            vec![crate::position::Position::Polar(
                crate::position::PolarPosition::new(30, 0, 1)
            )]
        );
        assert_eq!(e.element_gain_offset, None);
    }

    #[test]
    fn rendering_config_element_gain_offset_flag() {
        // Flag set: num_params 0, value-type offset -1 dB, then two bytes of
        // future extension, skipped.
        let config = [0x20, 0x06, 0x00, 0x00, 0xff, 0x00, 0xde, 0xad];
        let e = element_with_rendering_config(&config).unwrap();
        assert_eq!(
            e.element_gain_offset,
            Some(ElementGainOffsetConfig::Value { offset: -256 })
        );
        // Same bytes with the flag clear: they are future extension bytes.
        let config = [0x00, 0x04, 0x00, 0x00, 0xff, 0x00];
        let e = element_with_rendering_config(&config).unwrap();
        assert_eq!(e.element_gain_offset, None);
    }

    /// An extension that fails to parse (here: a non-position parameter
    /// type) is opaque and skipped whole, like iamf-tools.
    #[test]
    fn rendering_config_unknown_param_type_is_opaque() {
        for param_type in [0, 1, 2, 9] {
            // Flag set, but the gain offset sits behind the unknown param.
            let config = [0x20, 0x06, 0x01, param_type, 0x00, 0x00, 0xff, 0x00];
            let e = element_with_rendering_config(&config).unwrap();
            assert_eq!(e.position_params, []);
            assert_eq!(e.element_gain_offset, None);
        }
    }

    /// Content that would overrun `rendering_config_extension_size` is
    /// parsed only within its window, so it is opaque rather than an
    /// error, and the fields after the extension still parse.
    #[test]
    fn rendering_config_extension_overrun_is_opaque() {
        // rendering_config_extension_size 5, but the polar param alone
        // needs 10 bytes.
        let mut config = vec![0x00, 0x05, 0x01];
        config.extend(&POLAR_PARAM[..4]);
        let e = element_with_rendering_config(&config).unwrap();
        assert_eq!(e.position_params, []);
        assert_eq!(e.element_mix_gain.base.parameter_id, 0);
        // The gain offset overruns a 3-byte extension.
        let config = [0x20, 0x03, 0x00, 0x00, 0xff];
        let e = element_with_rendering_config(&config).unwrap();
        assert_eq!(e.element_gain_offset, None);
        // An extension larger than the rest of the payload.
        assert!(matches!(
            element_with_rendering_config(&[0x00, 0x7f]),
            Err(Error::UnexpectedEof { .. })
        ));
    }

    /// Regression: in IAMF v1.1 the extension bytes are reserved and
    /// SHOULD be ignored. A reserved byte 0x01 used to be parsed as a
    /// v2.0 `num_params`, read the following element_mix_gain (parameter
    /// id 3 reads as a polar definition) past the window, and reject the
    /// whole mix presentation.
    #[test]
    fn rendering_config_v1_reserved_extension_byte_ignored() {
        let mut payload = vec![0x2a, 0x00, 0x01, 0x01, 0x0a];
        payload.extend([0x00, 0x01, 0x01]); // mode 0, extension [0x01]
        for parameter_id in [0x03, 0x04] {
            payload.push(parameter_id);
            payload.extend([0x80, 0xf7, 0x02, 0x80, 0x00, 0x00]);
        }
        payload.extend([0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00]);
        let mp = MixPresentation::parse(&mut ByteReader::new(&payload)).unwrap();
        let e = &mp.sub_mixes[0].elements[0];
        assert_eq!(e.position_params, []);
        assert_eq!(e.element_mix_gain.base.parameter_id, 3);
        assert_eq!(mp.sub_mixes[0].output_mix_gain.base.parameter_id, 4);
    }

    /// Regression: the extension probe used to scan to the end of the
    /// OBU for every element, so parse time grew with the square of the
    /// element count (608 KB took ~29 s). Each element here carries an
    /// 11-byte extension whose `num_subblocks` claims far more subblock
    /// durations than the window holds.
    #[test]
    fn rendering_config_probe_is_linear() {
        const ELEMENTS: usize = 20_000;
        let mut payload = vec![0x2a, 0x00, 0x01];
        payload.extend(leb128_bytes(ELEMENTS as u32));
        for _ in 0..ELEMENTS {
            payload.extend([0x00, 0x00, 0x0b]); // element 0, mode 0, ext 11
            // num_params 1, polar, id 0, rate 0, mode 0, duration 0, csd 0,
            // then num_subblocks (4-byte leb128): small enough to pass the
            // count check for the first half of the elements, whose probes
            // then read subblock durations through the later elements.
            payload.extend([0x01, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00]);
            let n = (ELEMENTS as u32 / 2) * 19;
            payload.extend([
                (n & 0x7f) as u8 | 0x80,
                (n >> 7 & 0x7f) as u8 | 0x80,
                (n >> 14 & 0x7f) as u8 | 0x80,
                (n >> 21 & 0x7f) as u8,
            ]);
            payload.extend([0x00, 0x80, 0x00, 0x00, 0x00]); // element_mix_gain
        }
        payload.extend([0x00, 0x80, 0x00, 0x00, 0x00]); // output_mix_gain
        payload.extend([0x01, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00]);
        let start = std::time::Instant::now();
        let mp = MixPresentation::parse(&mut ByteReader::new(&payload)).unwrap();
        assert_eq!(mp.sub_mixes[0].elements.len(), ELEMENTS);
        // Well under a second even unoptimized; the quadratic version
        // took minutes.
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
    }

    fn leb128_bytes(mut value: u32) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return out;
            }
            out.push(byte | 0x80);
        }
    }

    /// Regression: list reservations are capped. A count that the input
    /// bytes allow, of items that are large in memory, used to reserve
    /// up to ~330x the OBU size at once (a 16 MB OBU asked for 5.6 GB).
    #[test]
    fn bounded_vec_reservation_is_capped() {
        fn byte(r: &mut ByteReader<'_>) -> Result<u8> {
            r.read_u8()
        }
        assert_eq!(preallocation(3), 3);
        assert_eq!(preallocation(1 << 30), MAX_PREALLOCATED_ITEMS);
        // Counts above the cap still parse.
        let data = vec![7u8; 10_000];
        let items = read_bounded_vec(&mut ByteReader::new(&data), 10_000, byte).unwrap();
        assert_eq!(items.len(), 10_000);
    }

    /// A projection demixing matrix larger than the remaining input is
    /// rejected before it is allocated.
    #[test]
    fn projection_matrix_checked_before_allocation() {
        // id 1, type 1 (scene-based), codec config 0, 1 substream id (0),
        // 0 params, mode 1 (projection), 255 outputs, 255 + 0 substreams.
        let payload = [
            0x01,
            1 << 5,
            0x00,
            0x01,
            0x00,
            0x00,
            0x01,
            0xff,
            0xff,
            0x00,
            0x00,
        ];
        assert!(matches!(
            AudioElement::parse(&mut ByteReader::new(&payload)),
            Err(Error::UnexpectedEof { .. })
        ));
    }

    fn parse_gain_offset(bytes: &[u8]) -> Result<ElementGainOffsetConfig> {
        let mut r = ByteReader::new(bytes);
        let config = ElementGainOffsetConfig::parse(&mut r)?;
        assert!(r.is_empty(), "trailing bytes");
        Ok(config)
    }

    fn gain_offset_range(default: i16, min: i16, max: i16) -> Result<ElementGainOffsetConfig> {
        let mut bytes = vec![0x01];
        for value in [default, min, max] {
            bytes.extend(value.to_be_bytes());
        }
        parse_gain_offset(&bytes)
    }

    #[test]
    fn element_gain_offset_value_and_range() {
        let value = parse_gain_offset(&[0x00, 0x01, 0x00]).unwrap();
        assert_eq!(value, ElementGainOffsetConfig::Value { offset: 256 });
        assert_eq!(value.default_offset(), Some(256));

        let range = gain_offset_range(0, -256, 256).unwrap();
        assert_eq!(
            range,
            ElementGainOffsetConfig::Range {
                default: 0,
                min: -256,
                max: 256,
            }
        );
        assert_eq!(range.default_offset(), Some(0));
        gain_offset_range(100, 100, 100).unwrap();

        // Truncated value and range.
        assert!(parse_gain_offset(&[0x00, 0x01]).is_err());
        assert!(parse_gain_offset(&[0x01, 0x00, 0x00, 0x00, 0x00]).is_err());
    }

    /// The range check compares absolute values (`min <= default <= max`),
    /// as iamf-tools does, not the spec v2.0 §3.7.1.1 offsets relative to
    /// the default.
    #[test]
    fn element_gain_offset_range_checks_absolute_values() {
        // Valid if min/max were relative to the default; rejected here.
        assert!(matches!(
            gain_offset_range(512, -256, 256),
            Err(Error::InvalidDescriptor { .. })
        ));
        // Accepted here; a negative relative max would be odd.
        gain_offset_range(-512, -1024, -256).unwrap();
        // min > max.
        assert!(matches!(
            gain_offset_range(0, 256, -256),
            Err(Error::InvalidDescriptor { .. })
        ));
        assert!(gain_offset_range(-257, -256, 256).is_err());
        assert!(gain_offset_range(257, -256, 256).is_err());
    }

    #[test]
    fn element_gain_offset_reserved_types() {
        let config = parse_gain_offset(&[0x02, 0x02, 0x01, 0x02]).unwrap();
        assert_eq!(
            config,
            ElementGainOffsetConfig::Extension {
                config_type: 2,
                bytes: vec![0x01, 0x02],
            }
        );
        assert_eq!(config.default_offset(), None);
        assert_eq!(
            parse_gain_offset(&[0xff, 0x00]).unwrap(),
            ElementGainOffsetConfig::Extension {
                config_type: 255,
                bytes: vec![],
            }
        );
        // Payload shorter than its size, and a missing size.
        assert!(matches!(
            parse_gain_offset(&[0x07, 0x03, 0x01]),
            Err(Error::UnexpectedEof { .. })
        ));
        assert!(matches!(
            parse_gain_offset(&[0x07]),
            Err(Error::UnexpectedEof { .. })
        ));
    }

    /// [`minimal_mix_payload`] plus an empty tags block and `optional`,
    /// parsed with `optional_fields_flag` set.
    fn mix_with_optional_fields(optional: &[u8]) -> Result<MixPresentation> {
        let mut payload = minimal_mix_payload(&[0x00, 0x00]);
        payload.push(0x00); // num_tags
        payload.extend_from_slice(optional);
        MixPresentation::parse_with_optional_fields(&mut ByteReader::new(&payload), true)
    }

    #[test]
    fn mix_presentation_optional_fields() {
        let mp = mix_with_optional_fields(&[0x02, 0x00, 0x00]).unwrap();
        assert_eq!(
            mp.optional_fields,
            Some(MixPresentationOptionalFields {
                preferred_loudspeaker_renderer: 0,
                preferred_binaural_renderer: 0,
                extension: vec![],
            })
        );
        let mp = mix_with_optional_fields(&[0x04, 0x01, 0x02, 0xaa, 0xbb]).unwrap();
        assert_eq!(
            mp.optional_fields,
            Some(MixPresentationOptionalFields {
                preferred_loudspeaker_renderer: 1,
                preferred_binaural_renderer: 2,
                extension: vec![0xaa, 0xbb],
            })
        );

        // Through descriptors::parse: the OBU header flag selects them.
        let mut payload = minimal_mix_payload(&[0x00, 0x00]);
        payload.extend([0x00, 0x02, 0x03, 0x04]);
        let mut data = vec![(2 << 3) | 0x02, u8::try_from(payload.len()).unwrap()];
        data.extend(&payload);
        let obu = Obu::parse(&mut ByteReader::new(&data)).unwrap();
        let Some(Descriptor::MixPresentation(mp)) = parse(&obu).unwrap() else {
            panic!("expected mix presentation");
        };
        let optional = mp.optional_fields.unwrap();
        assert_eq!(optional.preferred_loudspeaker_renderer, 3);
        assert_eq!(optional.preferred_binaural_renderer, 4);
    }

    #[test]
    fn mix_presentation_optional_fields_malformed() {
        // optional_fields_size must cover both renderer bytes.
        for optional in [&[0x00][..], &[0x01, 0x00]] {
            assert!(matches!(
                mix_with_optional_fields(optional),
                Err(Error::InvalidDescriptor { .. })
            ));
        }
        // Missing, or shorter than their size.
        assert!(matches!(
            mix_with_optional_fields(&[]),
            Err(Error::UnexpectedEof { .. })
        ));
        assert!(matches!(
            mix_with_optional_fields(&[0x03, 0x01, 0x02]),
            Err(Error::UnexpectedEof { .. })
        ));
        // Flagged, but the payload ends before the (mandatory) tags.
        let payload = minimal_mix_payload(&[0x00, 0x00]);
        assert!(matches!(
            MixPresentation::parse_with_optional_fields(&mut ByteReader::new(&payload), true),
            Err(Error::InvalidDescriptor { .. })
        ));
    }

    #[test]
    fn loudness_layout_extension_and_live() {
        // LIVE: integrated -16, digital peak -1, then info_type_bytes.
        let bytes = [0x04, 0xf0, 0x00, 0xff, 0x00, 0x02, 0xaa, 0xbb];
        let mut r = ByteReader::new(&bytes);
        let info = LoudnessInfo::parse(&mut r).unwrap();
        assert!(r.is_empty());
        assert!(info.is_live());
        assert_eq!(info.integrated_loudness, -4096);
        assert_eq!(info.digital_peak, -256);
        assert_eq!(info.true_peak, None);
        assert_eq!(info.anchored_loudness, []);
        assert_eq!(info.layout_extension, vec![0xaa, 0xbb]);

        // Another extension bit: sized region, not live.
        let mut r = ByteReader::new(&[0x80, 0, 0, 0, 0, 0x00]);
        let info = LoudnessInfo::parse(&mut r).unwrap();
        assert!(r.is_empty());
        assert!(!info.is_live());
        assert_eq!(info.layout_extension, []);

        // True peak, anchored loudness and LIVE together, in order.
        let bytes = [
            0x07, 0, 0, 0, 0, 0xff, 0x80, 0x01, 0x01, 0xff, 0x00, 0x01, 0x11,
        ];
        let mut r = ByteReader::new(&bytes);
        let info = LoudnessInfo::parse(&mut r).unwrap();
        assert!(r.is_empty());
        assert_eq!(info.true_peak, Some(-128));
        assert_eq!(info.anchored_loudness, vec![(1, -256)]);
        assert_eq!(info.layout_extension, vec![0x11]);

        // No extension bit: nothing more is read.
        let mut r = ByteReader::new(&[0x01, 0, 0, 0, 0, 0, 0, 0x05]);
        let info = LoudnessInfo::parse(&mut r).unwrap();
        assert_eq!(r.remaining(), 1);
        assert_eq!(info.layout_extension, []);

        // Extension size missing, or larger than the rest.
        for bytes in [&[0x04, 0, 0, 0, 0][..], &[0x04, 0, 0, 0, 0, 0x03, 0xaa]] {
            assert!(matches!(
                LoudnessInfo::parse(&mut ByteReader::new(bytes)),
                Err(Error::UnexpectedEof { .. })
            ));
        }
    }
}
