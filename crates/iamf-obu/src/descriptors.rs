//! Descriptor OBU payloads: IA sequence header, codec config, audio element,
//! and mix presentation (IAMF v1.1 §3.5–§3.8).
//!
//! Syntax targets IAMF v1.1 (the profile Chromium ships). Fields added by
//! later spec revisions arrive inside sized extension regions, which are
//! skipped rather than rejected, so v2-flavored streams still parse.

use crate::{ByteReader, Error, Obu, ObuType, Result};

/// A parsed descriptor OBU payload.
#[derive(Debug, Clone, PartialEq)]
pub enum Descriptor {
    SequenceHeader(SequenceHeader),
    CodecConfig(CodecConfig),
    AudioElement(AudioElement),
    MixPresentation(MixPresentation),
}

/// Parses the payload of a descriptor OBU. Returns `None` for OBU types that
/// are not descriptors (audio frames, parameter blocks, temporal delimiters).
pub fn parse(obu: &Obu<'_>) -> Result<Option<Descriptor>> {
    let mut r = ByteReader::new(obu.payload);
    let descriptor = match obu.header.obu_type {
        ObuType::SequenceHeader => Descriptor::SequenceHeader(SequenceHeader::parse(&mut r)?),
        ObuType::CodecConfig => Descriptor::CodecConfig(CodecConfig::parse(&mut r)?),
        ObuType::AudioElement => Descriptor::AudioElement(AudioElement::parse(&mut r)?),
        ObuType::MixPresentation => Descriptor::MixPresentation(MixPresentation::parse(&mut r)?),
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SequenceHeader {
    pub primary_profile: u8,
    pub additional_profile: u8,
}

impl SequenceHeader {
    pub fn parse(r: &mut ByteReader<'_>) -> Result<Self> {
        if &r.read_fourcc()? != b"iamf" {
            return Err(invalid(r));
        }
        Ok(SequenceHeader {
            primary_profile: r.read_u8()?,
            additional_profile: r.read_u8()?,
        })
    }
}

// ---------------------------------------------------------------------------
// Codec config (§3.6)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecId {
    Opus,
    AacLc,
    Flac,
    Lpcm,
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

/// Codec-specific decoder config (§3.6.1–§3.6.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecoderConfig {
    /// §3.6.1: OpusHead-equivalent fields, but big-endian.
    Opus {
        version: u8,
        output_channel_count: u8,
        pre_skip: u16,
        input_sample_rate: u32,
        output_gain: i16,
        mapping_family: u8,
    },
    /// §3.6.3: fields from the FLAC STREAMINFO metadata block, plus the
    /// raw 34-byte block body for codec initialization.
    Flac {
        sample_rate: u32,
        bits_per_sample: u8,
        streaminfo: Vec<u8>,
    },
    /// §3.6.4.
    Lpcm {
        little_endian: bool,
        sample_size: u8,
        sample_rate: u32,
    },
    /// §3.6.2: the AudioSpecificConfig extracted from the
    /// DecoderConfigDescriptor, for codec initialization.
    AacLc {
        audio_specific_config: Vec<u8>,
    },
    Unknown(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodecConfig {
    pub codec_config_id: u32,
    pub codec_id: CodecId,
    pub num_samples_per_frame: u32,
    pub audio_roll_distance: i16,
    pub decoder_config: DecoderConfig,
}

impl CodecConfig {
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
    pub parameter_id: u32,
    pub parameter_rate: u32,
    /// Mode 1: parameter blocks define their own timing. Mode 0: timing
    /// below applies.
    pub mode: bool,
    pub duration: u32,
    pub constant_subblock_duration: u32,
    pub subblock_durations: Vec<u32>,
}

impl ParamDefinition {
    fn parse(r: &mut ByteReader<'_>) -> Result<Self> {
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ElementParam {
    Demixing {
        base: ParamDefinition,
        default_demixing_mode: u8,
        default_weight_index: u8,
    },
    ReconGain(ParamDefinition),
    /// Reserved type; its sized definition was skipped.
    Unknown {
        param_type: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MixGainParam {
    pub base: ParamDefinition,
    /// Q7.8 dB.
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelAudioLayer {
    pub loudspeaker_layout: u8,
    pub substream_count: u8,
    pub coupled_substream_count: u8,
    pub recon_gain_is_present: bool,
    /// (output_gain_flags, output_gain Q7.8 dB) when present.
    pub output_gain: Option<(u8, i16)>,
    /// Present when the first layer's loudspeaker_layout is 15 (expanded).
    pub expanded_loudspeaker_layout: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioElementConfig {
    /// §3.7.4 scalable channel layout config.
    ChannelBased { layers: Vec<ChannelAudioLayer> },
    /// §3.7.5 ambisonics config, MONO mode.
    AmbisonicsMono {
        output_channel_count: u8,
        substream_count: u8,
        channel_mapping: Vec<u8>,
    },
    /// §3.7.5 ambisonics config, PROJECTION mode.
    AmbisonicsProjection {
        output_channel_count: u8,
        substream_count: u8,
        coupled_substream_count: u8,
        /// (substream_count + coupled_substream_count) rows of
        /// output_channel_count Q1.15 entries, decoded-channel-major
        /// (entry `[c * output_channel_count + acn]`).
        demixing_matrix: Vec<i16>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioElement {
    pub audio_element_id: u32,
    pub codec_config_id: u32,
    pub substream_ids: Vec<u32>,
    pub params: Vec<ElementParam>,
    pub config: AudioElementConfig,
}

impl AudioElement {
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
            _ => return Err(invalid(r)),
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
                let mut demixing_matrix = Vec::with_capacity(entries);
                for _ in 0..entries {
                    demixing_matrix.push(r.read_i16_be()?);
                }
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
}

// ---------------------------------------------------------------------------
// Mix presentation (§3.8)
// ---------------------------------------------------------------------------

/// §3.8.3 layout for loudness measurement / rendering targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// layout_type 2: ITU-R BS.2051 sound system letter (0..=9 → A..=J,
    /// then extensions).
    LoudspeakersSsConvention {
        sound_system: u8,
    },
    /// layout_type 3.
    Binaural,
    Reserved {
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
    pub info_type: u8,
    pub integrated_loudness: i16,
    pub digital_peak: i16,
    pub true_peak: Option<i16>,
    /// (anchor_element, anchored_loudness) pairs.
    pub anchored_loudness: Vec<(u8, i16)>,
}

impl LoudnessInfo {
    fn parse(r: &mut ByteReader<'_>) -> Result<Self> {
        const TRUE_PEAK: u8 = 0x01;
        const ANCHORED: u8 = 0x02;
        let info_type = r.read_u8()?;
        let integrated_loudness = r.read_i16_be()?;
        let digital_peak = r.read_i16_be()?;
        let true_peak = (info_type & TRUE_PEAK != 0)
            .then(|| r.read_i16_be())
            .transpose()?;
        let mut anchored_loudness = Vec::new();
        if info_type & ANCHORED != 0 {
            let count = r.read_u8()?;
            for _ in 0..count {
                anchored_loudness.push((r.read_u8()?, r.read_i16_be()?));
            }
        }
        if info_type & !(TRUE_PEAK | ANCHORED) != 0 {
            // Extension bits set: a sized extension region follows.
            let size = r.read_leb128()?;
            r.skip(size as usize)?;
        }
        Ok(LoudnessInfo {
            info_type,
            integrated_loudness,
            digital_peak,
            true_peak,
            anchored_loudness,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubMixElement {
    pub audio_element_id: u32,
    pub localized_annotations: Vec<String>,
    /// §3.8.2 rendering_config.
    pub headphones_rendering_mode: u8,
    pub element_mix_gain: MixGainParam,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubMix {
    pub elements: Vec<SubMixElement>,
    pub output_mix_gain: MixGainParam,
    /// Layouts the mix was authored/measured for, with loudness for each.
    pub layouts: Vec<(Layout, LoudnessInfo)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MixPresentation {
    pub mix_presentation_id: u32,
    /// BCP-47-ish language tags, one per label.
    pub annotation_languages: Vec<String>,
    pub localized_annotations: Vec<String>,
    pub sub_mixes: Vec<SubMix>,
    /// §8.x mix presentation tags (name, value), when present.
    pub tags: Vec<(String, String)>,
}

impl MixPresentation {
    pub fn parse(r: &mut ByteReader<'_>) -> Result<Self> {
        let mix_presentation_id = r.read_leb128()?;
        let count_label = r.read_leb128()?;
        let annotation_languages = read_bounded_vec(r, count_label, |r| r.read_string())?;
        let localized_annotations = read_bounded_vec(r, count_label, |r| r.read_string())?;

        let num_sub_mixes = r.read_leb128()?;
        let sub_mixes = read_bounded_vec(r, num_sub_mixes, |r| {
            let num_elements = r.read_leb128()?;
            if num_elements == 0 {
                return Err(invalid(r));
            }
            let elements = read_bounded_vec(r, num_elements, |r| {
                let audio_element_id = r.read_leb128()?;
                let localized_annotations = read_bounded_vec(r, count_label, |r| r.read_string())?;
                let headphones_rendering_mode = r.read_u8()? >> 6 & 0x03;
                let extension_size = r.read_leb128()?;
                r.skip(extension_size as usize)?;
                let element_mix_gain = MixGainParam::parse(r)?;
                Ok(SubMixElement {
                    audio_element_id,
                    localized_annotations,
                    headphones_rendering_mode,
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
            for _ in 0..num_tags {
                tags.push((r.read_string()?, r.read_string()?));
            }
        }

        Ok(MixPresentation {
            mix_presentation_id,
            annotation_languages,
            localized_annotations,
            sub_mixes,
            tags,
        })
    }
}

/// Reads `count` items, guarding against absurd counts from a hostile
/// bitstream: each item must consume at least one byte, so `count` can never
/// legitimately exceed the bytes remaining.
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
    let mut items = Vec::with_capacity(count);
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
        assert!(mp.tags.is_empty());
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
}
