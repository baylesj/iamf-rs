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

/// The IAMF profiles (iamf-tools `ProfileVersion`): the v1.1 simple (0),
/// base (1) and base-enhanced (2), and the v2.0 base-advanced (3),
/// advanced-1 (4) and advanced-2 (5), which add object-based elements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileSet(u8);

impl ProfileSet {
    /// IAMF Simple profile.
    pub const SIMPLE: ProfileSet = ProfileSet(1 << 0);
    /// IAMF Base profile.
    pub const BASE: ProfileSet = ProfileSet(1 << 1);
    /// IAMF Base-Enhanced profile.
    pub const BASE_ENHANCED: ProfileSet = ProfileSet(1 << 2);
    /// IAMF v2.0 Base-Advanced profile: object-only mixes.
    pub const BASE_ADVANCED: ProfileSet = ProfileSet(1 << 3);
    /// IAMF v2.0 Advanced-1 profile: objects mixed with channel-based and
    /// scene-based elements, up to 18 channels.
    pub const ADVANCED_1: ProfileSet = ProfileSet(1 << 4);
    /// IAMF v2.0 Advanced-2 profile: as Advanced-1, up to 28 channels.
    pub const ADVANCED_2: ProfileSet = ProfileSet(1 << 5);
    /// The IAMF v1.1 profiles, which have no object-based elements.
    pub const V1: ProfileSet = ProfileSet(0b111);
    /// The IAMF v2.0 profiles that carry object-based elements.
    pub const V2_OBJECTS: ProfileSet = ProfileSet(0b111_000);

    /// A profile set containing all known IAMF profiles.
    pub const fn all() -> Self {
        ProfileSet(0b111_111)
    }

    /// The profiles in both sets.
    #[must_use]
    pub const fn intersection(self, other: ProfileSet) -> Self {
        ProfileSet(self.0 & other.0)
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
    /// 2 = base-enhanced, 3 = base-advanced, 4 = advanced-1,
    /// 5 = advanced-2); unknown numbers map to the empty set.
    pub const fn from_profile_number(profile: u8) -> Self {
        match profile {
            0 => ProfileSet::SIMPLE,
            1 => ProfileSet::BASE,
            2 => ProfileSet::BASE_ENHANCED,
            3 => ProfileSet::BASE_ADVANCED,
            4 => ProfileSet::ADVANCED_1,
            5 => ProfileSet::ADVANCED_2,
            _ => ProfileSet::empty(),
        }
    }

    fn remove(&mut self, other: ProfileSet) {
        self.0 &= !other.0;
    }

    /// From the C-ABI / iamf-tools numbering: bit n = profile number n
    /// (simple, base, base-enhanced, base-advanced, advanced-1,
    /// advanced-2). Unknown high bits are ignored; an empty mask means "no
    /// constraint" and resolves to all known profiles.
    pub fn from_bits(bits: u32) -> Self {
        let known = (bits & 0b111_111) as u8;
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
                // groups, 9.1.6); 13..=19 arrived with the v2 draft
                // profiles and 20+ are reserved.
                15 => {
                    profiles.remove(ProfileSet::SIMPLE.union(ProfileSet::BASE));
                    match first.expanded_loudspeaker_layout {
                        Some(0..=12) => {}
                        _ => profiles.remove(ProfileSet::BASE_ENHANCED),
                    }
                }
                // 10..=14 are reserved in v1.1.
                _ => *profiles = ProfileSet::empty(),
            }
        }
        // MONO and PROJECTION ambisonics are allowed in every profile (our
        // parser rejects other modes outright).
        AudioElementConfig::AmbisonicsMono { .. }
        | AudioElementConfig::AmbisonicsProjection { .. } => {}
        // Objects arrived with the v2.0 profiles.
        AudioElementConfig::ObjectBased { .. } => profiles.remove(ProfileSet::V1),
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

    // Sub-mix count: v1.1 profiles all require exactly one.
    if mix.sub_mixes.len() != 1 {
        return ProfileSet::empty();
    }

    // headphones_rendering_mode: 0 and 1 are v1.1; 2 (head-locked binaural)
    // arrived with v2.0; 3 is reserved, and the mix must be ignored.
    for sub_mix in &mix.sub_mixes {
        for element in &sub_mix.elements {
            match element.headphones_rendering_mode {
                0 | 1 => {}
                2 => profiles.remove(ProfileSet::V1),
                _ => return ProfileSet::empty(),
            }
        }
    }

    let find_element = |id: u32| elements.iter().find(|e| e.audio_element_id == id);

    // Codec-config rules (spec §4): the first sub-mix must use exactly one
    // codec config under every v1.1 profile, which also pins a single
    // frame size and sample rate.
    let first_sub_mix_codec_configs: Vec<u32> = {
        let mut ids = Vec::new();
        for sub_element in &mix.sub_mixes[0].elements {
            let Some(element) = find_element(sub_element.audio_element_id) else {
                return ProfileSet::empty();
            };
            if codec_configs
                .iter()
                .all(|c| c.codec_config_id != element.codec_config_id)
            {
                return ProfileSet::empty();
            }
            if !ids.contains(&element.codec_config_id) {
                ids.push(element.codec_config_id);
            }
        }
        ids
    };
    if first_sub_mix_codec_configs.len() != 1 {
        return ProfileSet::empty();
    }

    // Per-element limits, plus element/channel budgets across the mix.
    let mut num_elements = 0usize;
    let mut num_channels = 0usize;
    let mut num_objects = 0usize;
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
            if matches!(element.config, AudioElementConfig::ObjectBased { .. }) {
                num_objects += 1;
            }
        }
    }
    // Base-advanced mixes objects only with objects; mixing them with
    // channel-based or scene-based elements takes advanced-1 or -2.
    if num_objects > 0 && num_objects < num_elements {
        profiles.remove(ProfileSet::BASE_ADVANCED);
    }
    if num_elements > 18 || num_channels > 18 {
        profiles.remove(ProfileSet::BASE_ADVANCED.union(ProfileSet::ADVANCED_1));
    }
    if num_elements > 28 || num_channels > 28 {
        profiles.remove(ProfileSet::ADVANCED_2);
    }
    if num_elements > 1 {
        profiles.remove(ProfileSet::SIMPLE);
    }
    if num_elements > 2 {
        profiles.remove(ProfileSet::BASE);
    }
    if num_elements > 28 {
        profiles.remove(ProfileSet::BASE_ENHANCED);
    }
    if num_channels > 16 {
        profiles.remove(ProfileSet::SIMPLE);
    }
    if num_channels > 18 {
        profiles.remove(ProfileSet::BASE);
    }
    if num_channels > 28 {
        profiles.remove(ProfileSet::BASE_ENHANCED);
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
                        binaural_filter_profile: 0,
                        position: None,
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
                    },
                )],
            }],
            tags: vec![],
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
        let set = filter_profiles_for_mix(&mix(&[1, 2], 0), &elements, &configs, ProfileSet::V1);
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
        let set =
            filter_profiles_for_mix(&mix(&[1], 0), &[element.clone()], &configs, ProfileSet::V1);
        assert_eq!(set, ProfileSet::BASE_ENHANCED);

        // v2-draft expanded layouts are outside every v1.1 profile.
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
        let set = filter_profiles_for_mix(&mix(&[1], 0), &[element], &configs, ProfileSet::V1);
        assert!(set.is_empty());
    }

    #[test]
    fn headlocked_binaural_needs_a_v2_profile() {
        let elements = [stereo_element(1, 0)];
        let configs = [codec_config(0)];
        let set = filter_profiles_for_mix(&mix(&[1], 2), &elements, &configs, ProfileSet::V1);
        assert!(set.is_empty());
        let set = filter_profiles_for_mix(&mix(&[1], 2), &elements, &configs, ProfileSet::all());
        assert_eq!(set, ProfileSet::V2_OBJECTS);
    }

    fn object_element(id: u32, codec: u32, num_objects: u8) -> AudioElement {
        let mut element = stereo_element(id, codec);
        element.config = AudioElementConfig::ObjectBased { num_objects };
        element
    }

    #[test]
    fn object_only_mixes_need_a_v2_profile() {
        let elements = [object_element(1, 0, 1), object_element(2, 0, 2)];
        let configs = [codec_config(0)];
        let set = filter_profiles_for_mix(&mix(&[1, 2], 0), &elements, &configs, ProfileSet::all());
        assert_eq!(set, ProfileSet::V2_OBJECTS);
        let set = filter_profiles_for_mix(&mix(&[1, 2], 0), &elements, &configs, ProfileSet::V1);
        assert!(set.is_empty());
    }

    #[test]
    fn objects_mixed_with_channels_need_advanced() {
        let elements = [object_element(1, 0, 1), stereo_element(2, 0)];
        let configs = [codec_config(0)];
        let set = filter_profiles_for_mix(&mix(&[1, 2], 0), &elements, &configs, ProfileSet::all());
        assert_eq!(set, ProfileSet::ADVANCED_1.union(ProfileSet::ADVANCED_2));
    }

    #[test]
    fn v2_profile_numbers_map_to_their_bits() {
        assert_eq!(
            ProfileSet::from_profile_number(3),
            ProfileSet::BASE_ADVANCED
        );
        assert_eq!(ProfileSet::from_profile_number(4), ProfileSet::ADVANCED_1);
        assert_eq!(ProfileSet::from_profile_number(5), ProfileSet::ADVANCED_2);
        assert_eq!(ProfileSet::from_bits(1 << 3), ProfileSet::BASE_ADVANCED);
    }

    #[test]
    fn two_codec_configs_in_first_sub_mix_unsupported() {
        let elements = [stereo_element(1, 0), stereo_element(2, 1)];
        let configs = [codec_config(0), codec_config(1)];
        let set = filter_profiles_for_mix(&mix(&[1, 2], 0), &elements, &configs, ProfileSet::all());
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
        assert_eq!(ProfileSet::from_bits(0b001), ProfileSet::SIMPLE);
        assert_eq!(
            ProfileSet::from_bits(0b110),
            ProfileSet::BASE.union(ProfileSet::BASE_ENHANCED)
        );
    }
}
