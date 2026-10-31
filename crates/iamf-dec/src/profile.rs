//! Profile capability filtering, ported from iamf-tools `ProfileFilter`
//! (`iamf/cli/profile_filter.cc`).
//!
//! iamf-tools does not compare the IA sequence header's declared profiles
//! against the caller's request. Instead, each mix presentation is checked
//! against the *limits* of every requested profile (element types, layer
//! layouts, sub-mix counts, codec-config rules, element/channel budgets);
//! a mix is decodable when at least one requested profile supports it. Mix
//! selection then only considers supported mixes.
//!
//! Known gap: only the per-mix rules are ported. iamf-tools' stream-level
//! checks are not: `ValidateMixPresentationsForPrimaryProfile`,
//! `ValidateFirstSubMixesUseSameCodecConfig`,
//! `ValidateAudioElementsForAdditionalProfile`, and the Base-Advanced rule
//! that the first mix presentation must be used.

use iamf_obu::descriptors::{
    AudioElement, AudioElementConfig, CodecConfig, CodecId, DecoderConfig, MixPresentation, SubMix,
    SubMixElement,
};

use crate::element::substream_channels;

/// The IAMF profiles (iamf-tools `ProfileVersion`):
/// - v1.1: Simple (0), Base (1), Base-Enhanced (2)
/// - v2.0: Base-Advanced (3), Advanced1 (4), Advanced2 (5)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileSet(u8);

impl ProfileSet {
    /// IAMF Simple profile (v1.1).
    pub const SIMPLE: ProfileSet = ProfileSet(1 << 0);
    /// IAMF Base profile (v1.1).
    pub const BASE: ProfileSet = ProfileSet(1 << 1);
    /// IAMF Base-Enhanced profile (v1.1).
    pub const BASE_ENHANCED: ProfileSet = ProfileSet(1 << 2);
    /// IAMF Base-Advanced profile (v2.0).
    pub const BASE_ADVANCED: ProfileSet = ProfileSet(1 << 3);
    /// IAMF Advanced1 profile (v2.0).
    pub const ADVANCED1: ProfileSet = ProfileSet(1 << 4);
    /// IAMF Advanced2 profile (v2.0).
    pub const ADVANCED2: ProfileSet = ProfileSet(1 << 5);

    /// A profile set containing all known IAMF v1.1 profiles. This is the
    /// default ([`ProfileSet::default`], [`ProfileSet::from_bits`]`(0)`):
    /// like iamf-tools `IamfDecoder::Settings`, the decoder only requests
    /// the profiles it can render.
    pub const fn all_v1() -> Self {
        ProfileSet(0b000111)
    }

    /// A profile set containing all known IAMF v1.1 and v2.0 profiles.
    /// The decoder cannot yet render everything the v2.0 profiles allow
    /// (several sub-mixes, object-based elements, v2.0 expanded layouts),
    /// so callers opt in to them explicitly.
    pub const fn all() -> Self {
        ProfileSet(0b111111)
    }

    /// An empty profile set.
    pub const fn empty() -> Self {
        ProfileSet(0)
    }

    /// Returns true if no profiles are included in this set.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Returns the union of this profile set and another.
    #[must_use]
    pub const fn union(self, other: ProfileSet) -> Self {
        ProfileSet(self.0 | other.0)
    }

    /// Returns true if this set shares at least one profile with `other`.
    pub const fn intersects(self, other: ProfileSet) -> bool {
        self.0 & other.0 != 0
    }

    /// From an IA sequence header profile number (0 = simple, 1 = base,
    /// 2 = base-enhanced, 3 = base-advanced, 4 = advanced1, 5 = advanced2);
    /// unknown numbers map to the empty set.
    pub const fn from_profile_number(profile: u8) -> Self {
        match profile {
            0 => ProfileSet::SIMPLE,
            1 => ProfileSet::BASE,
            2 => ProfileSet::BASE_ENHANCED,
            3 => ProfileSet::BASE_ADVANCED,
            4 => ProfileSet::ADVANCED1,
            5 => ProfileSet::ADVANCED2,
            _ => ProfileSet::empty(),
        }
    }

    fn remove(&mut self, other: ProfileSet) {
        self.0 &= !other.0;
    }

    /// From the C-ABI / iamf-tools numbering: bit 0 = simple, bit 1 = base,
    /// bit 2 = base-enhanced, bit 3 = base-advanced, bit 4 = advanced1, bit 5 = advanced2.
    /// Unknown high bits are ignored; an empty mask resolves to the default
    /// set, [`ProfileSet::all_v1`].
    pub fn from_bits(bits: u32) -> Self {
        let known = (bits & 0b111111) as u8;
        if known == 0 {
            ProfileSet::all_v1()
        } else {
            ProfileSet(known)
        }
    }
}

impl Default for ProfileSet {
    fn default() -> Self {
        ProfileSet::all_v1()
    }
}

/// samplingFrequencyIndex values of an AudioSpecificConfig (ISO/IEC
/// 14496-3 §1.6.3.4); 13 and 14 are reserved.
const AAC_SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

/// Sample rate of an AudioSpecificConfig: audioObjectType (5 bits), then
/// samplingFrequencyIndex (4 bits), then for the escape index 0xF an
/// explicit 24-bit samplingFrequency (iamf-tools
/// `AacDecoderConfig::GetOutputSampleRate`).
fn aac_sample_rate(asc: &[u8]) -> Option<u32> {
    let index = (u16::from_be_bytes([*asc.first()?, *asc.get(1)?]) >> 7) & 0x0f;
    let rate = if index == 0x0f {
        let bits = u32::from_be_bytes(asc.get(1..5)?.try_into().ok()?);
        (bits >> 7) & 0xff_ffff
    } else {
        *AAC_SAMPLE_RATES.get(usize::from(index))?
    };
    // §3.6.2 allows only the indexed rates (7350..=96000 Hz). The escape
    // index is accepted for those rates, as iamf-tools does, but not for
    // arbitrary explicit values (0 or MHz rates broke downstream stages).
    AAC_SAMPLE_RATES.contains(&rate).then_some(rate)
}

/// Output sample rate of substreams coded with `config`: 48 kHz for Opus,
/// the declared rate for LPCM and FLAC, and the AudioSpecificConfig rate
/// for AAC-LC. `None` for unknown codecs and malformed AAC configs.
pub fn output_sample_rate(config: &CodecConfig) -> Option<u32> {
    match &config.decoder_config {
        DecoderConfig::Opus { .. } => Some(48000),
        DecoderConfig::Lpcm { sample_rate, .. } | DecoderConfig::Flac { sample_rate, .. } => {
            Some(*sample_rate)
        }
        DecoderConfig::AacLc {
            audio_specific_config,
        } => aac_sample_rate(audio_specific_config),
        DecoderConfig::Unknown(_) => None,
    }
}

/// Decoded channel count of one element (what iamf-tools sums over
/// `substream_id_to_labels`).
fn element_channels(element: &AudioElement) -> usize {
    substream_channels(&element.config)
        .iter()
        .map(|&c| usize::from(c))
        .sum()
}

/// iamf-tools `FilterProfilesForAudioElement`: erases profiles whose limits
/// the element exceeds.
fn filter_audio_element(element: &AudioElement, profiles: &mut ProfileSet) {
    match &element.config {
        AudioElementConfig::ChannelBased { layers } => {
            let Some(first) = layers.first() else {
                *profiles = ProfileSet::empty();
                return;
            };
            match first.loudspeaker_layout {
                // Mono through binaural: allowed in every profile.
                0..=9 => {}
                // Expanded: never in simple/base; base-enhanced supports
                // expanded layouts 0..=12 (LFE/stereo subsets, top/front
                // groups, 9.1.6); 13..=19 (e.g. 10.2.9.3, 7.1.5.4) are supported
                // in Base-Advanced, Advanced1, and Advanced2.
                15 => {
                    profiles.remove(ProfileSet::SIMPLE.union(ProfileSet::BASE));
                    match first.expanded_loudspeaker_layout {
                        Some(0..=12) => {}
                        Some(13..=19) => {
                            profiles.remove(ProfileSet::BASE_ENHANCED);
                        }
                        _ => {
                            profiles.remove(
                                ProfileSet::BASE_ENHANCED
                                    .union(ProfileSet::BASE_ADVANCED)
                                    .union(ProfileSet::ADVANCED1)
                                    .union(ProfileSet::ADVANCED2),
                            );
                        }
                    }
                }
                // 10..=14 are reserved.
                _ => *profiles = ProfileSet::empty(),
            }
        }
        // MONO and PROJECTION ambisonics are allowed in every profile.
        AudioElementConfig::AmbisonicsMono { .. }
        | AudioElementConfig::AmbisonicsProjection { .. } => {}
        // Object-based elements (IAMF v2.0) are allowed in Base-Advanced, Advanced1, Advanced2.
        AudioElementConfig::ObjectBased { .. } => {
            profiles.remove(
                ProfileSet::SIMPLE
                    .union(ProfileSet::BASE)
                    .union(ProfileSet::BASE_ENHANCED),
            );
        }
        _ => *profiles = ProfileSet::empty(),
    }
}

/// iamf-tools `FilterRenderingConfigParamDefinitions`: an object-based
/// element needs exactly one position parameter carrying one position per
/// object (Polar/Cartesian8/Cartesian16 for one object, the Dual* types for
/// two); any other element must have none. Otherwise no profile applies.
fn filter_rendering_config(
    sub_element: &SubMixElement,
    element: &AudioElement,
    profiles: &mut ProfileSet,
) {
    let valid = match &element.config {
        AudioElementConfig::ObjectBased { num_objects, .. } => matches!(
            sub_element.position_params.as_slice(),
            [param] if param.param_type.num_positions() == usize::from(*num_objects)
        ),
        _ => sub_element.position_params.is_empty(),
    };
    if !valid {
        *profiles = ProfileSet::empty();
    }
}

/// iamf-tools `ProfileFilter::FilterProfilesForMixPresentation`: returns the
/// subset of `requested` profiles that support this mix presentation.
/// Elements referenced by the mix but missing from `elements`, or codec
/// configs missing from `codec_configs`, yield an empty set.
pub fn filter_profiles_for_mix(
    mix: &MixPresentation,
    elements: &[AudioElement],
    codec_configs: &[CodecConfig],
    requested: ProfileSet,
) -> ProfileSet {
    let mut profiles = requested;

    // Sub-mix count: v1.1 profiles require exactly 1. v2 profiles allow up to 2.
    if mix.sub_mixes.len() > 1 {
        profiles.remove(
            ProfileSet::SIMPLE
                .union(ProfileSet::BASE)
                .union(ProfileSet::BASE_ENHANCED),
        );
    }
    if mix.sub_mixes.len() > 2 || mix.sub_mixes.is_empty() {
        return ProfileSet::empty();
    }

    // §3.7: num_audio_elements SHALL NOT be 0, and there is nothing to
    // render without an element (iamf-tools refuses to, ac2fff70b).
    if mix
        .sub_mixes
        .iter()
        .any(|sub_mix| sub_mix.elements.is_empty())
    {
        return ProfileSet::empty();
    }

    // headphones_rendering_mode: 0 and 1 are allowed in all profiles;
    // 2 (head-locked binaural) is supported in v2 profiles (Base-Advanced, Advanced1, Advanced2).
    for sub_mix in &mix.sub_mixes {
        for element in &sub_mix.elements {
            match element.headphones_rendering_mode {
                0 | 1 => {}
                2 => profiles.remove(
                    ProfileSet::SIMPLE
                        .union(ProfileSet::BASE)
                        .union(ProfileSet::BASE_ENHANCED),
                ),
                _ => return ProfileSet::empty(),
            }
        }
    }

    let find_element = |id: u32| elements.iter().find(|e| e.audio_element_id == id);
    let find_codec = |id: u32| codec_configs.iter().find(|c| c.codec_config_id == id);

    // Codec-config rules (spec §4, iamf-tools FilterProfilesForCodecConfigRules):
    // 1. Condition A: First sub-mix must use exactly one Codec Config.
    // 2. Condition B: Max unique Codec Configs per mix: v1.1 = 1, v2 = 2.
    // 3. Condition C: If two unique Codec Configs, at least one must be LPCM.
    // 4. Condition D: All Codec Configs in mix must have matching sample_rate & frame_size.
    let mut all_codec_ids = Vec::new();
    let mut first_sub_mix_codec_ids = Vec::new();

    for (i, sub_mix) in mix.sub_mixes.iter().enumerate() {
        for sub_element in &sub_mix.elements {
            let Some(element) = find_element(sub_element.audio_element_id) else {
                return ProfileSet::empty();
            };
            let Some(cc) = find_codec(element.codec_config_id) else {
                return ProfileSet::empty();
            };
            if !all_codec_ids.contains(&cc.codec_config_id) {
                all_codec_ids.push(cc.codec_config_id);
            }
            if i == 0 && !first_sub_mix_codec_ids.contains(&cc.codec_config_id) {
                first_sub_mix_codec_ids.push(cc.codec_config_id);
            }
        }
    }

    // Condition A check: first sub-mix has >1 codec config -> unsupported by all.
    if first_sub_mix_codec_ids.len() > 1 {
        return ProfileSet::empty();
    }

    // Condition B check:
    if all_codec_ids.len() > 1 {
        profiles.remove(
            ProfileSet::SIMPLE
                .union(ProfileSet::BASE)
                .union(ProfileSet::BASE_ENHANCED),
        );
    }
    if all_codec_ids.len() > 2 {
        return ProfileSet::empty();
    }

    // Condition C & D check if 2 codec configs:
    if all_codec_ids.len() == 2 {
        let Some(c1) = find_codec(all_codec_ids[0]) else {
            return ProfileSet::empty();
        };
        let Some(c2) = find_codec(all_codec_ids[1]) else {
            return ProfileSet::empty();
        };
        // Condition C: At least one must be LPCM
        if c1.codec_id != CodecId::Lpcm && c2.codec_id != CodecId::Lpcm {
            return ProfileSet::empty();
        }
        // Condition D: Same frame sizes and sample rates (an unknown rate
        // never matches).
        let (Some(sr1), Some(sr2)) = (output_sample_rate(c1), output_sample_rate(c2)) else {
            return ProfileSet::empty();
        };
        let (fs1, fs2) = (c1.num_samples_per_frame, c2.num_samples_per_frame);
        if sr1 != sr2 || fs1 != fs2 {
            return ProfileSet::empty();
        }
    }

    // Per-element limits, plus element/channel budgets across the mix.
    let mut num_elements = 0usize;
    let mut num_channels = 0usize;
    let mut has_object = false;
    let mut has_non_object = false;
    for sub_mix in &mix.sub_mixes {
        num_elements += sub_mix.elements.len();
        for sub_element in &sub_mix.elements {
            let Some(element) = find_element(sub_element.audio_element_id) else {
                return ProfileSet::empty();
            };
            filter_audio_element(element, &mut profiles);
            filter_rendering_config(sub_element, element, &mut profiles);
            if profiles.is_empty() {
                return profiles;
            }
            if matches!(element.config, AudioElementConfig::ObjectBased { .. }) {
                has_object = true;
            } else {
                has_non_object = true;
            }
            num_channels += element_channels(element);
        }
    }

    // Base-Advanced does not allow mixing object-based and other elements
    // in one mix presentation.
    if has_object && has_non_object {
        profiles.remove(ProfileSet::BASE_ADVANCED);
    }

    // Audio element count budgets:
    // Simple: 1, Base: 2, Base-Enhanced: 28, Base-Advanced: 18, Advanced1: 18, Advanced2: 28.
    if num_elements > 1 {
        profiles.remove(ProfileSet::SIMPLE);
    }
    if num_elements > 2 {
        profiles.remove(ProfileSet::BASE);
    }
    if num_elements > 18 {
        profiles.remove(ProfileSet::BASE_ADVANCED.union(ProfileSet::ADVANCED1));
    }
    if num_elements > 28 {
        profiles.remove(ProfileSet::BASE_ENHANCED.union(ProfileSet::ADVANCED2));
    }

    // Channel budgets:
    // Simple: 16, Base: 18, Base-Enhanced: 28, Base-Advanced: 18, Advanced1: 18, Advanced2: 28.
    if num_channels > 16 {
        profiles.remove(ProfileSet::SIMPLE);
    }
    if num_channels > 18 {
        profiles.remove(
            ProfileSet::BASE
                .union(ProfileSet::BASE_ADVANCED)
                .union(ProfileSet::ADVANCED1),
        );
    }
    if num_channels > 28 {
        profiles.remove(ProfileSet::BASE_ENHANCED.union(ProfileSet::ADVANCED2));
    }
    profiles
}

/// The single codec config a supported mix resolves to (valid once
/// [`filter_profiles_for_mix`] returned non-empty for it).
pub fn mix_codec_config<'a>(
    sub_mix: &SubMix,
    elements: &[AudioElement],
    codec_configs: &'a [CodecConfig],
) -> Option<&'a CodecConfig> {
    let first_id = sub_mix.elements.first()?.audio_element_id;
    let element = elements.iter().find(|e| e.audio_element_id == first_id)?;
    codec_configs
        .iter()
        .find(|c| c.codec_config_id == element.codec_config_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use iamf_obu::ByteReader;
    use iamf_obu::descriptors::{ChannelAudioLayer, LoudnessInfo, MixGainParam, ParamDefinition};
    use iamf_obu::position::{PositionParamDefinition, PositionParamType};

    const V2: ProfileSet = ProfileSet::BASE_ADVANCED
        .union(ProfileSet::ADVANCED1)
        .union(ProfileSet::ADVANCED2);

    fn gain() -> MixGainParam {
        MixGainParam {
            base: ParamDefinition {
                parameter_id: 0,
                parameter_rate: 48000,
                mode: true,
                duration: 0,
                constant_subblock_duration: 0,
                subblock_durations: vec![],
            },
            default_mix_gain: 0,
        }
    }

    fn codec_config(id: u32) -> CodecConfig {
        CodecConfig {
            codec_config_id: id,
            codec_id: CodecId::Lpcm,
            num_samples_per_frame: 64,
            audio_roll_distance: 0,
            decoder_config: DecoderConfig::Lpcm {
                little_endian: true,
                sample_size: 16,
                sample_rate: 48000,
            },
        }
    }

    fn stereo_element(id: u32, codec: u32) -> AudioElement {
        AudioElement {
            audio_element_id: id,
            codec_config_id: codec,
            substream_ids: vec![id * 10],
            params: vec![],
            config: AudioElementConfig::ChannelBased {
                layers: vec![ChannelAudioLayer {
                    loudspeaker_layout: 1,
                    substream_count: 1,
                    coupled_substream_count: 1,
                    recon_gain_is_present: false,
                    output_gain: None,
                    expanded_loudspeaker_layout: None,
                }],
            },
        }
    }

    fn mix(element_ids: &[u32], headphones_mode: u8) -> MixPresentation {
        MixPresentation {
            mix_presentation_id: 1,
            annotation_languages: vec![],
            localized_annotations: vec![],
            sub_mixes: vec![SubMix {
                elements: element_ids
                    .iter()
                    .map(|&id| SubMixElement {
                        audio_element_id: id,
                        localized_annotations: vec![],
                        headphones_rendering_mode: headphones_mode,
                        binaural_filter_profile:
                            iamf_obu::descriptors::BinauralFilterProfile::default(),
                        position_params: vec![],
                        element_gain_offset: None,
                        element_mix_gain: gain(),
                    })
                    .collect(),
                output_mix_gain: gain(),
                layouts: vec![(
                    iamf_obu::descriptors::Layout::LoudspeakersSsConvention { sound_system: 0 },
                    LoudnessInfo {
                        info_type: 0,
                        integrated_loudness: 0,
                        digital_peak: 0,
                        true_peak: None,
                        anchored_loudness: vec![],
                        layout_extension: vec![],
                    },
                )],
            }],
            tags: vec![],
            optional_fields: None,
        }
    }

    /// `mix(element_ids, 0)` whose elements all declare `params`.
    fn mix_with_params(element_ids: &[u32], params: &[PositionParamDefinition]) -> MixPresentation {
        let mut m = mix(element_ids, 0);
        for element in &mut m.sub_mixes[0].elements {
            element.position_params = params.to_vec();
        }
        m
    }

    /// `mix(first, 0)` plus a second sub-mix holding `second`.
    fn two_sub_mix(first: &[u32], second: &[u32]) -> MixPresentation {
        let mut m = mix(first, 0);
        m.sub_mixes.push(mix(second, 0).sub_mixes.remove(0));
        m
    }

    /// A position parameter definition (id 7, rate 1, mode 1) with zeroed
    /// default positions.
    fn position_param(param_type: PositionParamType) -> PositionParamDefinition {
        // Enough zero bytes for two 16-bit cartesian positions.
        let mut bytes = vec![7, 1, 0x80];
        bytes.extend([0; 12]);
        PositionParamDefinition::parse(&mut ByteReader::new(&bytes), param_type).unwrap()
    }

    fn object_element(id: u32, num_objects: u8) -> AudioElement {
        AudioElement {
            audio_element_id: id,
            codec_config_id: 0,
            substream_ids: vec![id * 10],
            params: vec![],
            config: AudioElementConfig::ObjectBased {
                num_objects,
                extension: vec![],
            },
        }
    }

    fn mono_element(id: u32) -> AudioElement {
        let mut element = stereo_element(id, 0);
        element.config = AudioElementConfig::ChannelBased {
            layers: vec![ChannelAudioLayer {
                loudspeaker_layout: 0,
                substream_count: 1,
                coupled_substream_count: 0,
                recon_gain_is_present: false,
                output_gain: None,
                expanded_loudspeaker_layout: None,
            }],
        };
        element
    }

    fn lpcm_config(id: u32, sample_rate: u32, frame: u32) -> CodecConfig {
        let mut config = codec_config(id);
        config.num_samples_per_frame = frame;
        config.decoder_config = DecoderConfig::Lpcm {
            little_endian: true,
            sample_size: 16,
            sample_rate,
        };
        config
    }

    fn opus_config(id: u32, frame: u32) -> CodecConfig {
        CodecConfig {
            codec_config_id: id,
            codec_id: CodecId::Opus,
            num_samples_per_frame: frame,
            audio_roll_distance: -4,
            decoder_config: DecoderConfig::Opus {
                version: 1,
                output_channel_count: 2,
                pre_skip: 312,
                input_sample_rate: 48000,
                output_gain: 0,
                mapping_family: 0,
            },
        }
    }

    fn flac_config(id: u32, frame: u32) -> CodecConfig {
        CodecConfig {
            codec_config_id: id,
            codec_id: CodecId::Flac,
            num_samples_per_frame: frame,
            audio_roll_distance: 0,
            decoder_config: DecoderConfig::Flac {
                sample_rate: 48000,
                bits_per_sample: 16,
                streaminfo: vec![],
            },
        }
    }

    fn aac_config(id: u32, frame: u32, audio_specific_config: Vec<u8>) -> CodecConfig {
        CodecConfig {
            codec_config_id: id,
            codec_id: CodecId::AacLc,
            num_samples_per_frame: frame,
            audio_roll_distance: -1,
            decoder_config: DecoderConfig::AacLc {
                audio_specific_config,
            },
        }
    }

    /// AudioSpecificConfig: AAC-LC (object type 2), 48 kHz (index 3),
    /// stereo.
    const ASC_48K: [u8; 2] = [0x11, 0x90];

    /// Profiles supporting a two-sub-mix mix whose sub-mixes hold element
    /// 1 (codec config `first`) and element 2 (codec config `second`).
    fn two_codec_profiles(first: CodecConfig, second: CodecConfig) -> ProfileSet {
        let elements = [
            stereo_element(1, first.codec_config_id),
            stereo_element(2, second.codec_config_id),
        ];
        filter_profiles_for_mix(
            &two_sub_mix(&[1], &[2]),
            &elements,
            &[first, second],
            ProfileSet::all(),
        )
    }

    #[test]
    fn output_sample_rates() {
        assert_eq!(output_sample_rate(&opus_config(0, 960)), Some(48000));
        assert_eq!(output_sample_rate(&flac_config(0, 960)), Some(48000));
        assert_eq!(output_sample_rate(&lpcm_config(0, 44100, 960)), Some(44100));
        assert_eq!(
            output_sample_rate(&aac_config(0, 1024, ASC_48K.to_vec())),
            Some(48000)
        );
        // 44.1 kHz (index 4).
        assert_eq!(
            output_sample_rate(&aac_config(0, 1024, vec![0x12, 0x10])),
            Some(44100)
        );
        // Escape index 0xF with an explicit 24-bit rate (here 48000 =
        // 0x00BB80): 00010 1111 000000001011101110000000 0010...
        assert_eq!(
            output_sample_rate(&aac_config(0, 1024, vec![0x17, 0x80, 0x5d, 0xc0, 0x10])),
            Some(48000)
        );
        // Regression: the explicit rate must be one of the indexed rates;
        // 0 and 4.8 MHz used to pass and crash or stall the binaural
        // resampler.
        for asc in [
            vec![0x17, 0x80, 0x00, 0x00, 0x10],
            vec![0x17, 0x84, 0x93, 0xe0, 0x10],
        ] {
            assert_eq!(output_sample_rate(&aac_config(0, 1024, asc)), None);
        }
        // Truncated, reserved index 13, and truncated escape.
        for asc in [vec![0x11], vec![0x16, 0x90], vec![0x17, 0x80, 0x5d]] {
            assert_eq!(
                output_sample_rate(&aac_config(0, 1024, asc.clone())),
                None,
                "{asc:x?}"
            );
        }
        let mut unknown = codec_config(0);
        unknown.codec_id = CodecId::Unknown(*b"abcd");
        unknown.decoder_config = DecoderConfig::Unknown(vec![]);
        assert_eq!(output_sample_rate(&unknown), None);
    }

    #[test]
    fn lpcm_plus_lossy_codec_in_second_sub_mix_needs_v2() {
        assert_eq!(
            two_codec_profiles(lpcm_config(0, 48000, 960), opus_config(1, 960)),
            V2
        );
        // AAC-LC's rate comes from its AudioSpecificConfig.
        assert_eq!(
            two_codec_profiles(
                lpcm_config(0, 48000, 1024),
                aac_config(1, 1024, ASC_48K.to_vec())
            ),
            V2
        );
        // v1.1 profiles allow neither two sub-mixes nor two codec configs.
        let elements = [stereo_element(1, 0), stereo_element(2, 1)];
        let configs = [lpcm_config(0, 48000, 960), opus_config(1, 960)];
        let set = filter_profiles_for_mix(
            &two_sub_mix(&[1], &[2]),
            &elements,
            &configs,
            ProfileSet::all_v1(),
        );
        assert!(set.is_empty());
    }

    /// Condition C: one of two codec configs must be LPCM.
    #[test]
    fn two_lossy_codec_configs_unsupported() {
        assert!(two_codec_profiles(opus_config(0, 960), flac_config(1, 960)).is_empty());
    }

    /// Condition D: matching sample rates and frame sizes.
    #[test]
    fn mismatched_codec_configs_unsupported() {
        // Frame size.
        assert!(two_codec_profiles(lpcm_config(0, 48000, 480), opus_config(1, 960)).is_empty());
        // Sample rate.
        assert!(two_codec_profiles(lpcm_config(0, 44100, 960), opus_config(1, 960)).is_empty());
        assert!(
            two_codec_profiles(
                lpcm_config(0, 44100, 1024),
                aac_config(1, 1024, ASC_48K.to_vec())
            )
            .is_empty()
        );
        // Unknown AAC rate.
        assert!(
            two_codec_profiles(
                lpcm_config(0, 48000, 1024),
                aac_config(1, 1024, vec![0x16, 0x90])
            )
            .is_empty()
        );
    }

    #[test]
    fn three_codec_configs_unsupported() {
        let elements = [
            stereo_element(1, 0),
            stereo_element(2, 1),
            stereo_element(3, 2),
        ];
        let configs = [
            lpcm_config(0, 48000, 960),
            lpcm_config(1, 48000, 960),
            lpcm_config(2, 48000, 960),
        ];
        let set = filter_profiles_for_mix(
            &two_sub_mix(&[1], &[2, 3]),
            &elements,
            &configs,
            ProfileSet::all(),
        );
        assert!(set.is_empty());
    }

    #[test]
    fn three_sub_mixes_unsupported() {
        let elements = [stereo_element(1, 0)];
        let mut m = two_sub_mix(&[1], &[1]);
        m.sub_mixes.push(m.sub_mixes[0].clone());
        let set = filter_profiles_for_mix(&m, &elements, &[codec_config(0)], ProfileSet::all());
        assert!(set.is_empty());
    }

    #[test]
    fn reserved_headphones_rendering_mode_unsupported() {
        let elements = [stereo_element(1, 0)];
        let set = filter_profiles_for_mix(
            &mix(&[1], 3),
            &elements,
            &[codec_config(0)],
            ProfileSet::all(),
        );
        assert!(set.is_empty());
    }

    /// Element budgets: Base-Advanced and Advanced1 allow 18, Advanced2
    /// (and Base-Enhanced) 28. Mono elements, so channels equal elements.
    #[test]
    fn v2_element_budget_boundaries() {
        let configs = [codec_config(0)];
        let profiles = |count: u32| {
            let ids: Vec<u32> = (1..=count).collect();
            let elements: Vec<AudioElement> = ids.iter().map(|&id| mono_element(id)).collect();
            filter_profiles_for_mix(&mix(&ids, 0), &elements, &configs, ProfileSet::all())
        };
        assert_eq!(profiles(18), ProfileSet::BASE_ENHANCED.union(V2));
        assert_eq!(
            profiles(19),
            ProfileSet::BASE_ENHANCED.union(ProfileSet::ADVANCED2)
        );
        assert_eq!(
            profiles(28),
            ProfileSet::BASE_ENHANCED.union(ProfileSet::ADVANCED2)
        );
        assert!(profiles(29).is_empty());
    }

    /// Channel budgets with few elements: a 25-channel fourth-order
    /// ambisonics element plus mono elements.
    #[test]
    fn v2_channel_budget_boundaries() {
        let configs = [codec_config(0)];
        let profiles = |num_mono: u32| {
            let mut elements = vec![AudioElement {
                audio_element_id: 100,
                codec_config_id: 0,
                substream_ids: (0..25).collect(),
                params: vec![],
                config: AudioElementConfig::AmbisonicsMono {
                    output_channel_count: 25,
                    substream_count: 25,
                    channel_mapping: (0..25).collect(),
                },
            }];
            elements.extend((1..=num_mono).map(mono_element));
            let ids: Vec<u32> = elements.iter().map(|e| e.audio_element_id).collect();
            filter_profiles_for_mix(&mix(&ids, 0), &elements, &configs, ProfileSet::all())
        };
        assert_eq!(
            profiles(3),
            ProfileSet::BASE_ENHANCED.union(ProfileSet::ADVANCED2)
        );
        assert!(profiles(4).is_empty());
    }

    #[test]
    fn stereo_mix_supported_by_all_profiles() {
        let elements = [stereo_element(1, 0)];
        let configs = [codec_config(0)];
        let set = filter_profiles_for_mix(&mix(&[1], 0), &elements, &configs, ProfileSet::all());
        assert_eq!(set, ProfileSet::all());
    }

    #[test]
    fn two_elements_exceed_simple() {
        let elements = [stereo_element(1, 0), stereo_element(2, 0)];
        let configs = [codec_config(0)];
        let set =
            filter_profiles_for_mix(&mix(&[1, 2], 0), &elements, &configs, ProfileSet::all_v1());
        assert_eq!(set, ProfileSet::BASE.union(ProfileSet::BASE_ENHANCED));
        // Requesting only simple leaves nothing.
        let set =
            filter_profiles_for_mix(&mix(&[1, 2], 0), &elements, &configs, ProfileSet::SIMPLE);
        assert!(set.is_empty());
    }

    #[test]
    fn expanded_layout_needs_base_enhanced() {
        let mut element = stereo_element(1, 0);
        element.config = AudioElementConfig::ChannelBased {
            layers: vec![ChannelAudioLayer {
                loudspeaker_layout: 15,
                substream_count: 1,
                coupled_substream_count: 0,
                recon_gain_is_present: false,
                output_gain: None,
                expanded_loudspeaker_layout: Some(0), // LFE subset
            }],
        };
        let configs = [codec_config(0)];
        let set = filter_profiles_for_mix(
            &mix(&[1], 0),
            &[element.clone()],
            &configs,
            ProfileSet::all_v1(),
        );
        assert_eq!(set, ProfileSet::BASE_ENHANCED);

        // v2 expanded layouts are supported in Base-Advanced, Advanced1, Advanced2.
        element.config = AudioElementConfig::ChannelBased {
            layers: vec![ChannelAudioLayer {
                loudspeaker_layout: 15,
                substream_count: 1,
                coupled_substream_count: 0,
                recon_gain_is_present: false,
                output_gain: None,
                expanded_loudspeaker_layout: Some(13), // 10.2.9.3
            }],
        };
        let set = filter_profiles_for_mix(&mix(&[1], 0), &[element], &configs, ProfileSet::all());
        assert_eq!(
            set,
            ProfileSet::BASE_ADVANCED
                .union(ProfileSet::ADVANCED1)
                .union(ProfileSet::ADVANCED2)
        );
    }

    #[test]
    fn headlocked_binaural_supported_in_v2_profiles() {
        let elements = [stereo_element(1, 0)];
        let configs = [codec_config(0)];
        let set = filter_profiles_for_mix(&mix(&[1], 2), &elements, &configs, ProfileSet::all());
        assert_eq!(
            set,
            ProfileSet::BASE_ADVANCED
                .union(ProfileSet::ADVANCED1)
                .union(ProfileSet::ADVANCED2)
        );
        let set = filter_profiles_for_mix(&mix(&[1], 2), &elements, &configs, ProfileSet::all_v1());
        assert!(set.is_empty());
    }

    #[test]
    fn object_element_supported_in_v2_profiles() {
        let configs = [codec_config(0)];
        for (num_objects, param_type) in [
            (1, PositionParamType::Polar),
            (1, PositionParamType::Cartesian8),
            (1, PositionParamType::Cartesian16),
            (2, PositionParamType::DualPolar),
            (2, PositionParamType::DualCartesian8),
            (2, PositionParamType::DualCartesian16),
        ] {
            let elements = [object_element(1, num_objects)];
            let m = mix_with_params(&[1], &[position_param(param_type)]);
            let set = filter_profiles_for_mix(&m, &elements, &configs, ProfileSet::all());
            assert_eq!(set, V2, "{param_type:?}");
            let set = filter_profiles_for_mix(&m, &elements, &configs, ProfileSet::all_v1());
            assert!(set.is_empty(), "{param_type:?}");
        }
    }

    /// iamf-tools `FilterRenderingConfigParamDefinitions`.
    #[test]
    fn object_element_needs_one_matching_position_param() {
        let configs = [codec_config(0)];
        let one = [object_element(1, 1)];
        let two = [object_element(1, 2)];
        let polar = || position_param(PositionParamType::Polar);
        let dual = || position_param(PositionParamType::DualPolar);
        for (elements, params) in [
            (&one, vec![]),
            (&one, vec![dual()]),
            (&one, vec![polar(), polar()]),
            (&two, vec![]),
            (&two, vec![polar()]),
        ] {
            let m = mix_with_params(&[1], &params);
            let set = filter_profiles_for_mix(&m, elements, &configs, ProfileSet::all());
            assert!(
                set.is_empty(),
                "{:?}",
                m.sub_mixes[0].elements[0].position_params
            );
        }
    }

    #[test]
    fn non_object_element_with_position_param_unsupported() {
        let m = mix_with_params(&[1], &[position_param(PositionParamType::Polar)]);
        let set = filter_profiles_for_mix(
            &m,
            &[stereo_element(1, 0)],
            &[codec_config(0)],
            ProfileSet::all(),
        );
        assert!(set.is_empty());
    }

    #[test]
    fn mixed_object_and_channel_elements_exclude_base_advanced() {
        let elements = [object_element(1, 1), stereo_element(2, 0)];
        let configs = [codec_config(0)];
        let polar = || vec![position_param(PositionParamType::Polar)];
        let mut m = mix(&[1, 2], 0);
        m.sub_mixes[0].elements[0].position_params = polar();
        let set = filter_profiles_for_mix(&m, &elements, &configs, ProfileSet::all());
        assert_eq!(set, ProfileSet::ADVANCED1.union(ProfileSet::ADVANCED2));
        // Likewise across sub-mixes.
        let mut m = two_sub_mix(&[1], &[2]);
        m.sub_mixes[0].elements[0].position_params = polar();
        let set = filter_profiles_for_mix(&m, &elements, &configs, ProfileSet::all());
        assert_eq!(set, ProfileSet::ADVANCED1.union(ProfileSet::ADVANCED2));
    }

    #[test]
    fn two_codec_configs_in_first_sub_mix_unsupported_all() {
        let elements = [stereo_element(1, 0), stereo_element(2, 1)];
        let configs = [codec_config(0), codec_config(1)];
        let set = filter_profiles_for_mix(&mix(&[1, 2], 0), &elements, &configs, ProfileSet::all());
        assert!(set.is_empty());
    }

    #[test]
    fn two_submixes_supported_in_v2_profiles() {
        let elements = [stereo_element(1, 0), stereo_element(2, 0)];
        let configs = [codec_config(0)];
        let mut m = mix(&[1], 0);
        let mut sub2 = m.sub_mixes[0].clone();
        sub2.elements[0].audio_element_id = 2;
        m.sub_mixes.push(sub2);
        let set = filter_profiles_for_mix(&m, &elements, &configs, ProfileSet::all());
        assert_eq!(
            set,
            ProfileSet::BASE_ADVANCED
                .union(ProfileSet::ADVANCED1)
                .union(ProfileSet::ADVANCED2)
        );
        let set = filter_profiles_for_mix(&m, &elements, &configs, ProfileSet::all_v1());
        assert!(set.is_empty());
    }

    #[test]
    fn missing_element_reference_unsupported() {
        let configs = [codec_config(0)];
        let set = filter_profiles_for_mix(&mix(&[9], 0), &[], &configs, ProfileSet::all());
        assert!(set.is_empty());
    }

    #[test]
    fn sub_mix_without_elements_unsupported() {
        let elements = [stereo_element(1, 0)];
        let configs = [codec_config(0)];
        let set = filter_profiles_for_mix(&mix(&[], 0), &elements, &configs, ProfileSet::all());
        assert!(set.is_empty());
    }

    #[test]
    fn profile_bits_roundtrip() {
        // No constraint means the default, v1.1-only set: v2 is opt-in.
        assert_eq!(ProfileSet::from_bits(0), ProfileSet::all_v1());
        assert_eq!(ProfileSet::default(), ProfileSet::all_v1());
        assert_eq!(ProfileSet::from_bits(0b111111), ProfileSet::all());
        assert_eq!(ProfileSet::from_bits(0b000001), ProfileSet::SIMPLE);
        assert_eq!(
            ProfileSet::from_bits(0b000110),
            ProfileSet::BASE.union(ProfileSet::BASE_ENHANCED)
        );
        assert_eq!(
            ProfileSet::from_bits(0b111000),
            ProfileSet::BASE_ADVANCED
                .union(ProfileSet::ADVANCED1)
                .union(ProfileSet::ADVANCED2)
        );
    }
}
