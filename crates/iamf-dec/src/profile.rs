//! Profile capability filtering, ported from iamf-tools `ProfileFilter`
//! (`iamf/cli/profile_filter.cc`).
//!
//! iamf-tools does not compare the IA sequence header's declared profiles
//! against the caller's request. Instead, each mix presentation is checked
//! against the *limits* of every requested profile (element types, layer
//! layouts, sub-mix counts, codec-config rules, element/channel budgets);
//! a mix is decodable when at least one requested profile supports it. Mix
//! selection then only considers supported mixes.

use iamf_obu::descriptors::{
    AudioElement, AudioElementConfig, CodecConfig, MixPresentation, SubMix,
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

    /// A profile set containing all known IAMF v1.1 profiles.
    pub const fn all_v1() -> Self {
        ProfileSet(0b000111)
    }

    /// A profile set containing all known IAMF v1.1 and v2.0 profiles.
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
    /// Unknown high bits are ignored; an empty mask means "no constraint" and
    /// resolves to all known profiles.
    pub fn from_bits(bits: u32) -> Self {
        let known = (bits & 0b111111) as u8;
        if known == 0 {
            ProfileSet::all()
        } else {
            ProfileSet(known)
        }
    }
}

impl Default for ProfileSet {
    fn default() -> Self {
        ProfileSet::all()
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

    // headphones_rendering_mode: 0 and 1 are allowed in all profiles;
    // 2 (head-locked binaural) is supported in v2 profiles (Base-Advanced, Advanced1, Advanced2).
    for sub_mix in &mix.sub_mixes {
        for element in &sub_mix.elements {
            if element.headphones_rendering_mode == 2 {
                profiles.remove(
                    ProfileSet::SIMPLE
                        .union(ProfileSet::BASE)
                        .union(ProfileSet::BASE_ENHANCED),
                );
            } else if element.headphones_rendering_mode > 2 {
                return ProfileSet::empty();
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
        if c1.codec_id != iamf_obu::descriptors::CodecId::Lpcm
            && c2.codec_id != iamf_obu::descriptors::CodecId::Lpcm
        {
            return ProfileSet::empty();
        }
        // Condition D: Same frame sizes and sample rates
        let sr1 = match &c1.decoder_config {
            iamf_obu::descriptors::DecoderConfig::Opus { .. } => 48000,
            iamf_obu::descriptors::DecoderConfig::Lpcm { sample_rate, .. }
            | iamf_obu::descriptors::DecoderConfig::Flac { sample_rate, .. } => *sample_rate,
            _ => 0,
        };
        let sr2 = match &c2.decoder_config {
            iamf_obu::descriptors::DecoderConfig::Opus { .. } => 48000,
            iamf_obu::descriptors::DecoderConfig::Lpcm { sample_rate, .. }
            | iamf_obu::descriptors::DecoderConfig::Flac { sample_rate, .. } => *sample_rate,
            _ => 0,
        };
        let (fs1, fs2) = (c1.num_samples_per_frame, c2.num_samples_per_frame);
        if sr1 != sr2 || fs1 != fs2 {
            return ProfileSet::empty();
        }
    }

    // Per-element limits, plus element/channel budgets across the mix.
    let mut num_elements = 0usize;
    let mut num_channels = 0usize;
    for sub_mix in &mix.sub_mixes {
        num_elements += sub_mix.elements.len();
        for sub_element in &sub_mix.elements {
            let Some(element) = find_element(sub_element.audio_element_id) else {
                return ProfileSet::empty();
            };
            filter_audio_element(element, &mut profiles);
            if profiles.is_empty() {
                return profiles;
            }
            num_channels += element_channels(element);
        }
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
    use iamf_obu::descriptors::{
        ChannelAudioLayer, CodecId, DecoderConfig, LoudnessInfo, MixGainParam, ParamDefinition,
        SubMixElement,
    };

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
        let element = AudioElement {
            audio_element_id: 1,
            codec_config_id: 0,
            substream_ids: vec![10],
            params: vec![],
            config: AudioElementConfig::ObjectBased {
                num_objects: 1,
                extension: vec![],
            },
        };
        let configs = [codec_config(0)];
        let set = filter_profiles_for_mix(&mix(&[1], 0), &[element], &configs, ProfileSet::all());
        assert_eq!(
            set,
            ProfileSet::BASE_ADVANCED
                .union(ProfileSet::ADVANCED1)
                .union(ProfileSet::ADVANCED2)
        );
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
    fn profile_bits_roundtrip() {
        assert_eq!(ProfileSet::from_bits(0), ProfileSet::all());
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
