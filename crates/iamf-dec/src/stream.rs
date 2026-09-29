//! Iterative (streaming) decoder, shaped after the iamf-tools decoder API
//! that Chromium's `IamfAudioDecoder` consumes: configure from a
//! descriptor blob, push arbitrary byte chunks (whole or partial OBUs),
//! and pull decoded temporal units as interleaved little-endian PCM.

use std::collections::VecDeque;

use iamf_obu::descriptors::{AudioElement, AudioElementConfig, CodecConfig, ElementParam, SubMix};
use iamf_obu::{AudioFrame, ByteReader, Error, Obu, ObuType};

use crate::element::{FramePcm, substream_channels};
use crate::layout::{
    LOUDSPEAKER_LAYOUT_BINAURAL, LOUDSPEAKER_LAYOUT_STEREO, SoundSystem, is_binaural_input,
};
use crate::params::{
    ParamContext, ParamCursor, ParamIndex, ParamKind, ParameterBlock, ReconGainLayers,
    SubblockData, build_param_index,
};
use crate::post::{LIMITER_LOOKAHEAD, LIMITER_THRESHOLD_DB, PeakLimiter};
use crate::presentation::Descriptors;
use crate::profile::ProfileSet;
use crate::reconstruct::{ChannelReconstructor, ambisonics_from_planes, deinterleave};
use crate::render::render;
use crate::{CodecFactory, DecodeError, DecodedFrame, SubstreamDecoder};

/// Output PCM encoding (iamf-tools `OutputSampleType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OutputSampleType {
    /// 16-bit signed integer little-endian PCM.
    Int16LittleEndian,
    /// 32-bit signed integer little-endian PCM.
    Int32LittleEndian,
}

impl OutputSampleType {
    /// Number of bytes per audio sample for this encoding (2 for s16, 4 for s32).
    pub fn bytes_per_sample(self) -> usize {
        match self {
            OutputSampleType::Int16LittleEndian => 2,
            OutputSampleType::Int32LittleEndian => 4,
        }
    }
}

/// Output channel ordering (iamf-tools `ChannelOrdering`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChannelOrdering {
    /// IAMF rendering order, as the sound systems define it.
    #[default]
    Iamf,
    /// Android AudioFormat / WAVE order (matches iamf-tools
    /// `kOrderingForAndroid`).
    Android,
}

/// Frame trimming control (iamf-tools `TrimmingSettings`): disable when an
/// outer layer (e.g. an MP4 demuxer honoring edts/elst) trims instead.
/// Non-exhaustive: construct via `Default` and set fields.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct TrimmingSettings {
    /// Whether to apply leading sample trims from audio frame OBUs.
    pub trim_beginning: bool,
    /// Whether to apply trailing sample trims from audio frame OBUs.
    pub trim_end: bool,
}

impl Default for TrimmingSettings {
    fn default() -> Self {
        TrimmingSettings {
            trim_beginning: true,
            trim_end: true,
        }
    }
}

/// Non-exhaustive so future knobs are not breaking: construct via
/// [`StreamSettings::default`] and set the fields you need.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct StreamSettings {
    /// Target loudspeaker layout or binaural rendering mode.
    pub layout: SoundSystem,
    /// `None` selects s16le or s32le from the stream's bit depth.
    pub sample_type: Option<OutputSampleType>,
    /// Which mix presentation to decode.
    pub mix_selection: MixSelection,
    /// Output channel ordering (IAMF standard or Android/WAVE).
    pub channel_ordering: ChannelOrdering,
    /// Trimming configuration for packet start/end trims.
    pub trimming: TrimmingSettings,
    /// Profiles the caller supports (iamf-tools
    /// `requested_profile_versions`): the stream's declared profiles must
    /// intersect this set, and only mix presentations within some requested
    /// profile's limits are selectable.
    pub requested_profiles: ProfileSet,
    /// Loudness normalization target in dB (LKFS): applies a constant gain
    /// of `target - content` using the selected layout's `loudness_info`.
    /// `None` (the default) disables normalization, matching the iamf-tools
    /// decoder, which ignores loudness metadata.
    pub loudness_target_db: Option<f32>,
    /// libiamf-style look-ahead peak limiter at -1 dBFS. Off by default:
    /// the iamf-tools decoder (Chromium's reference) emits unlimited
    /// rendered PCM, while libiamf limits by default — integrators choose.
    pub enable_limiter: bool,
}

/// Mix presentation selection (iamf-tools `RequestedMix` shape).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum MixSelection {
    /// Creator-preferred selection for the requested output layout (IAMF
    /// §7.4.1, as in iamf-tools `FindMixPresentationAndLayout`): for
    /// binaural output, a mix whose single audio element is authored
    /// binaural, else one declaring a binaural loudness layout; for stereo
    /// (sound system A), a mix with one stereo layout and one stereo
    /// element, preferring `headphones_rendering_mode` stereo; for other
    /// layouts, the first mix declaring that layout. Falls back to the
    /// first supported mix. Mixes that cannot be rendered to the requested
    /// layout (binaural-input elements to loudspeakers other than stereo)
    /// are never selected.
    #[default]
    Auto,
    /// Select by mix_presentation_id. When no supported mix carries the id,
    /// selection proceeds as if unspecified, i.e. like [`MixSelection::Auto`]
    /// (iamf-tools `RequestedMix` semantics).
    ById(u32),
    /// Select by position in the descriptors (must be supported).
    ByIndex(usize),
}

impl Default for StreamSettings {
    fn default() -> Self {
        StreamSettings {
            layout: SoundSystem::A,
            sample_type: Some(OutputSampleType::Int16LittleEndian),
            mix_selection: MixSelection::Auto,
            channel_ordering: ChannelOrdering::default(),
            trimming: TrimmingSettings::default(),
            requested_profiles: ProfileSet::all(),
            loudness_target_db: None,
            enable_limiter: false,
        }
    }
}

/// Output channel permutation for a target layout and ordering
/// (iamf-tools `ChannelReorderer`): entry i is the rendered-channel index
/// written to interleaved slot i.
fn output_permutation(target: SoundSystem, ordering: ChannelOrdering) -> Vec<usize> {
    let identity = |n: usize| (0..n).collect::<Vec<_>>();
    let channels = target.channels();
    if ordering == ChannelOrdering::Iamf {
        return identity(channels);
    }
    match target {
        // [L, R, C, LFE, Lss, Rss, Lrs, Rrs, ...]: Android wants rears
        // before sides.
        SoundSystem::I | SoundSystem::J | SoundSystem::Ext712 => {
            let mut p = identity(channels);
            p.swap(4, 6);
            p.swap(5, 7);
            p
        }
        // [C, L, R, LH, RH, LS, RS, LB, RB, CH, LFE1, LFE2]
        SoundSystem::F => vec![1, 2, 0, 10, 7, 8, 5, 6, 9, 3, 4, 11],
        // [L, R, C, LFE, Lss, Rss, Lrs, Rrs, Ltf, Rtf, Ltb, Rtb, Lsc, Rsc]
        SoundSystem::G => vec![0, 1, 2, 3, 6, 7, 12, 13, 4, 5, 8, 9, 10, 11],
        // BS.2051 H (9+10+3), see iamf-tools ReorderSoundSystemHForAndroid.
        SoundSystem::H => vec![
            0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 11, 15, 12, 14, 13, 16, 20, 17, 18, 19, 22, 21, 23, 9,
        ],
        // Everything else matches Android order already.
        _ => identity(channels),
    }
}

/// §3.8.2 `headphones_rendering_mode` for stereo (non-HRTF) playback.
const HEADPHONES_RENDERING_MODE_STEREO: u8 = 0;

/// Whether `sub_mix` has exactly one audio element, and that element is
/// scalable-channel-based with its last (highest) layer in
/// `loudspeaker_layout` (iamf-tools `HasOneAudioElementWithLayout`).
/// Elements missing from `elements` never match.
fn has_one_element_with_layout(
    sub_mix: &SubMix,
    elements: &[AudioElement],
    loudspeaker_layout: u8,
) -> bool {
    let [sub_element] = sub_mix.elements.as_slice() else {
        return false;
    };
    elements
        .iter()
        .find(|e| e.audio_element_id == sub_element.audio_element_id)
        .is_some_and(|e| match &e.config {
            AudioElementConfig::ChannelBased { layers } => layers
                .last()
                .is_some_and(|l| l.loudspeaker_layout == loudspeaker_layout),
            _ => false,
        })
}

/// §7.4.1 2.2.x candidate: the first sub-mix declares exactly one layout,
/// sound system A (0+2+0), and has exactly one stereo channel-based
/// element (iamf-tools `HasOneStereoLayoutAndAudioElement`). The spec is
/// silent about multiple sub-mixes; like iamf-tools, only the first is
/// considered so that an appended "system sound" sub-mix does not change
/// the choice.
fn has_one_stereo_layout_and_element(
    mix: &iamf_obu::descriptors::MixPresentation,
    elements: &[AudioElement],
) -> bool {
    use iamf_obu::descriptors::Layout;
    let Some(first) = mix.sub_mixes.first() else {
        return false;
    };
    matches!(
        first.layouts.as_slice(),
        [(Layout::LoudspeakersSsConvention { sound_system: 0 }, _)]
    ) && has_one_element_with_layout(first, elements, LOUDSPEAKER_LAYOUT_STEREO)
}

/// Resolves a mix selection against parsed descriptors, following the
/// creator-preferred selection of IAMF §7.4.1 as implemented by
/// iamf-tools `FindMixPresentationAndLayout`. `supported[i]` says whether
/// mix i fits some requested profile (see
/// [`crate::profile::filter_profiles_for_mix`]) and can be rendered to
/// `target`; unsupported mixes are never selected. `elements` are the stream's
/// audio element descriptors, inspected by the binaural and stereo
/// clauses.
///
/// Over the supported mixes, in descriptor order:
/// 1. [`MixSelection::ById`]: the mix with that id, if supported.
/// 2. By `target` layout:
///    - Binaural: 2.1.1 the first mix whose first sub-mix has exactly one
///      element, a channel-based one whose last layer is BINAURAL; else
///      2.1.2 the first mix declaring a binaural loudness layout. (2.1.3,
///      "highest layout" fallback, is unimplemented upstream too.)
///    - Stereo (sound system A): 2.2.1 the first mix satisfying
///      [`has_one_stereo_layout_and_element`] whose element has
///      `headphones_rendering_mode` 0 (stereo); else 2.2.2 the same
///      without the rendering-mode constraint.
///    - Otherwise: 2.3.1 the first mix declaring `target` in any sub-mix.
/// 3. Otherwise the first supported mix.
///
/// A 2.1.1 mix carries binaural (loudspeaker_layout 9) input, which is
/// passed through to binaural (or stereo) output. Such mixes cannot be
/// rendered to other loudspeaker layouts, so callers mark them
/// unsupported for those targets (see
/// [`Descriptors::select_mix_presentation`]).
pub(crate) fn select_mix_index(
    mixes: &[iamf_obu::descriptors::MixPresentation],
    elements: &[AudioElement],
    supported: &[bool],
    selection: MixSelection,
    target: SoundSystem,
) -> Result<usize, DecodeError> {
    use iamf_obu::descriptors::Layout;
    if let MixSelection::ByIndex(index) = selection {
        return match supported.get(index) {
            Some(true) => Ok(index),
            Some(false) => Err(DecodeError::UnsupportedProfile(format!(
                "mix presentation {index} exceeds the requested profiles or cannot be \
                 rendered to {target:?}"
            ))),
            None => Err(DecodeError::InvalidDescriptors(
                "no such mix presentation".into(),
            )),
        };
    }
    let is_supported = |i: usize| supported.get(i).copied().unwrap_or(false);
    let first_where = |pred: &dyn Fn(&iamf_obu::descriptors::MixPresentation) -> bool| {
        mixes
            .iter()
            .enumerate()
            .position(|(i, m)| is_supported(i) && pred(m))
    };
    if let MixSelection::ById(id) = selection {
        // A missing or unsupported id falls back to automatic selection
        // (iamf-tools `RequestedMix`: "the decoder will behave as if it
        // was unspecified").
        if let Some(index) = first_where(&|m| m.mix_presentation_id == id) {
            return Ok(index);
        }
    }
    let by_layout = match target {
        SoundSystem::Binaural => {
            // 2.1.1: exactly one element, authored binaural.
            first_where(&|m| {
                m.sub_mixes.first().is_some_and(|sm| {
                    has_one_element_with_layout(sm, elements, LOUDSPEAKER_LAYOUT_BINAURAL)
                })
            })
            // 2.1.2: a binaural loudness layout in any sub-mix. Mixes that
            // only declare stereo are deliberately not preferred.
            .or_else(|| {
                first_where(&|m| {
                    m.sub_mixes.iter().any(|sm| {
                        sm.layouts
                            .iter()
                            .any(|(layout, _)| matches!(layout, Layout::Binaural))
                    })
                })
            })
        }
        SoundSystem::A => {
            // 2.2.1: one stereo layout + one stereo element, rendered as
            // stereo on headphones.
            first_where(&|m| {
                has_one_stereo_layout_and_element(m, elements)
                    && m.sub_mixes[0].elements[0].headphones_rendering_mode
                        == HEADPHONES_RENDERING_MODE_STEREO
            })
            // 2.2.2: same, any headphones_rendering_mode.
            .or_else(|| first_where(&|m| has_one_stereo_layout_and_element(m, elements)))
        }
        // 2.3.1: the first mix declaring the exact target layout.
        other => first_where(&|m| {
            m.sub_mixes.iter().any(|sm| {
                sm.layouts.iter().any(|(layout, _)| match layout {
                    Layout::LoudspeakersSsConvention { sound_system } => {
                        SoundSystem::from_u8(*sound_system) == Some(other)
                    }
                    Layout::Binaural | Layout::Reserved { .. } => false,
                })
            })
        }),
    };
    // 3: the first supported mix.
    by_layout
        .or_else(|| (0..mixes.len()).find(|&i| is_supported(i)))
        .ok_or_else(|| {
            DecodeError::UnsupportedProfile(
                "no mix presentation fits the requested profiles and output layout".into(),
            )
        })
}

/// Per-sample animated gain cursor: consumes subblocks in arrival order,
/// falling back to the default gain when exhausted. Animations are stored
/// with endpoints pre-converted to linear gain.
#[derive(Default)]
struct GainCursor {
    queue: VecDeque<(crate::params::LinearAnimation, usize, usize)>,
}

impl GainCursor {
    fn push(&mut self, anim: &crate::params::MixGainAnimation, duration: usize) {
        if duration > 0 {
            self.queue
                .push_back((crate::params::LinearAnimation::from(anim), duration, 0));
        }
    }

    fn next(&mut self, default: f32) -> f32 {
        let Some((anim, duration, pos)) = self.queue.front_mut() else {
            return default;
        };
        let gain = anim.evaluate_at(*duration, *pos);
        *pos += 1;
        if *pos >= *duration {
            self.queue.pop_front();
        }
        gain
    }
}

struct SlotState {
    element: AudioElement,
    codec_config: CodecConfig,
    substream_ids: Vec<u32>,
    channels: Vec<u8>,
    decoders: Vec<Box<dyn SubstreamDecoder>>,
    /// One decoded-frame queue per substream.
    queues: Vec<VecDeque<FramePcm>>,
    /// Demixing-mode timeline (dmixp_mode per covered temporal unit).
    dmx_cursor: ParamCursor<u8>,
    /// Recon-gain timeline.
    recon_cursor: ParamCursor<ReconGainLayers>,
    reconstructor: Option<ChannelReconstructor>,
    /// §3.8.2: 0 = stereo fallback for headphones, 1 = HRTF binaural.
    headphones_rendering_mode: u8,
    #[cfg(feature = "binaural")]
    binaural: Option<crate::binaural::BinauralRenderer>,
    gain_default: f32,
    gain_cursor: GainCursor,
    sample_rate: u32,
}

impl SlotState {
    fn unit_ready(&self) -> bool {
        self.queues.iter().all(|q| !q.is_empty())
    }
}

/// Feeds one temporal unit's planes through a slot's stateful binaural
/// renderer (created on first use with this unit's frame length).
#[cfg(feature = "binaural")]
fn binauralize_unit(
    renderer: &mut Option<crate::binaural::BinauralRenderer>,
    input: crate::binaural::BinauralInput,
    planes: &[Vec<f32>],
    frame_len: usize,
    sample_rate: u32,
) -> Result<Vec<Vec<f32>>, DecodeError> {
    if renderer.is_none() {
        *renderer = Some(crate::binaural::BinauralRenderer::new(
            input,
            frame_len,
            sample_rate,
        )?);
    }
    let r = renderer.as_mut().expect("created above");
    let chunk: Vec<Vec<f32>> = planes
        .iter()
        .map(|p| {
            let mut c = p.clone();
            c.resize(frame_len.max(p.len()), 0.0);
            c
        })
        .collect();
    let [l, right] = r.process(&chunk)?;
    Ok(vec![
        l[..frame_len.min(l.len())].to_vec(),
        right[..frame_len.min(right.len())].to_vec(),
    ])
}

/// The selected layout's `loudness_info` integrated loudness, in dB
/// (Q7.8 → dB). Falls back to the first measured layout when none matches.
fn content_loudness_db(sub_mix: &SubMix, target: SoundSystem) -> Option<f32> {
    use iamf_obu::descriptors::Layout;
    let matches = |layout: &Layout| match layout {
        Layout::LoudspeakersSsConvention { sound_system } => {
            SoundSystem::from_u8(*sound_system) == Some(target)
        }
        Layout::Binaural => target == SoundSystem::Binaural,
        Layout::Reserved { .. } => false,
    };
    sub_mix
        .layouts
        .iter()
        .find(|(l, _)| matches(l))
        .or_else(|| sub_mix.layouts.first())
        .map(|(_, info)| f32::from(info.integrated_loudness) / 256.0)
}

impl core::fmt::Debug for StreamDecoder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StreamDecoder")
            .field("selected_mix_id", &self.selected_mix_id)
            .field("target", &self.target)
            .field("elements", &self.slots.len())
            .finish_non_exhaustive()
    }
}

/// Streaming IAMF decoder for one mix presentation and output layout.
pub struct StreamDecoder {
    slots: Vec<SlotState>,
    /// See [`ParamIndex`].
    param_index: ParamIndex,
    target: SoundSystem,
    sample_type: OutputSampleType,
    /// Output channel permutation: slot i of the interleaved output takes
    /// rendered channel `permutation[i]`.
    permutation: Vec<usize>,
    settings: StreamSettings,
    selected_mix_id: u32,
    output_gain_default: f32,
    output_cursor: GainCursor,
    /// Constant linear gain from loudness normalization (1.0 when off).
    norm_gain: f32,
    /// Streaming peak limiter, created at the first pulled unit (it needs
    /// the resolved sample rate).
    limiter: Option<PeakLimiter>,
    /// Buffered bytes of a partially received OBU.
    pending: Vec<u8>,
    frame_size: u32,
    ended: bool,
    /// Parsed descriptors, retained for [`StreamDecoder::reset_with_new_mix`].
    parsed: Descriptors,
}

impl StreamDecoder {
    /// Creates a decoder from a descriptor blob (the descriptor OBUs of an
    /// IA sequence, e.g. from an ISO-BMFF `iacb` config box).
    pub fn new_from_descriptors(
        descriptors: &[u8],
        settings: StreamSettings,
        factory: &dyn CodecFactory,
    ) -> Result<Self, DecodeError> {
        let parsed = Descriptors::collect(descriptors)?;
        Self::from_parsed(parsed, settings, factory, &mut Vec::new())
    }

    /// Builds a configured decoder, harvesting matching codec decoders from
    /// `reuse` (element id → decoders) instead of creating new ones.
    fn from_parsed(
        parsed: Descriptors,
        settings: StreamSettings,
        factory: &dyn CodecFactory,
        reuse: &mut Vec<(u32, Vec<Box<dyn SubstreamDecoder>>)>,
    ) -> Result<Self, DecodeError> {
        if parsed.mix_presentations.is_empty() {
            return Err(DecodeError::InvalidDescriptors(
                "no mix presentations".into(),
            ));
        }
        // §3.5: decode only when the stream declares a profile we were
        // asked to support (checked when the blob includes the header).
        if let Some(header) = &parsed.sequence_header {
            let declared = ProfileSet::from_profile_number(header.primary_profile)
                .union(ProfileSet::from_profile_number(header.additional_profile));
            if !declared.intersects(settings.requested_profiles) {
                return Err(DecodeError::UnsupportedProfile(format!(
                    "stream declares profiles {}/{} outside the requested set",
                    header.primary_profile, header.additional_profile
                )));
            }
        }
        // iamf-tools semantics: a mix presentation is selectable when it
        // fits within some requested profile's limits (and, here, can be
        // rendered to the target); among those, §7.4.1 creator-preferred
        // selection applies.
        let mix_index = parsed.select_mix_presentation(
            settings.mix_selection,
            settings.layout,
            settings.requested_profiles,
        )?;
        let mix = &parsed.mix_presentations[mix_index];
        let [sub_mix] = mix.sub_mixes.as_slice() else {
            // Guaranteed by the profile filter; kept as a defensive check.
            return Err(DecodeError::InvalidDescriptors(
                "IAMF v1.1 requires exactly one sub mix per mix presentation".into(),
            ));
        };

        let mut slots = Vec::new();
        let mut frame_size = 0u32;
        for sub_element in &sub_mix.elements {
            let element = parsed
                .audio_elements
                .iter()
                .find(|e| e.audio_element_id == sub_element.audio_element_id)
                .ok_or_else(|| {
                    DecodeError::InvalidDescriptors(format!(
                        "mix references unknown element {}",
                        sub_element.audio_element_id
                    ))
                })?;
            let codec_config = parsed
                .codec_configs
                .iter()
                .find(|c| c.codec_config_id == element.codec_config_id)
                .ok_or_else(|| {
                    DecodeError::InvalidDescriptors(format!(
                        "element references unknown codec config {}",
                        element.codec_config_id
                    ))
                })?;
            if !factory.supports(codec_config) {
                return Err(DecodeError::UnsupportedCodec);
            }
            frame_size = codec_config.num_samples_per_frame;
            let channels = substream_channels(&element.config);
            if channels.is_empty() || channels.len() != element.substream_ids.len() {
                return Err(DecodeError::InvalidDescriptors(
                    "substream count mismatch".into(),
                ));
            }
            let reused = reuse
                .iter()
                .position(|(id, decoders)| {
                    *id == element.audio_element_id && decoders.len() == channels.len()
                })
                .map(|i| reuse.swap_remove(i).1);
            let decoders = match reused {
                Some(mut decoders) => {
                    for d in &mut decoders {
                        d.reset();
                    }
                    decoders
                }
                None => channels
                    .iter()
                    .map(|&ch| factory.create(codec_config, ch))
                    .collect::<Result<Vec<_>, _>>()?,
            };

            let queues = vec![VecDeque::new(); channels.len()];
            slots.push(SlotState {
                element: element.clone(),
                codec_config: codec_config.clone(),
                headphones_rendering_mode: sub_element.headphones_rendering_mode,
                #[cfg(feature = "binaural")]
                binaural: None,
                substream_ids: element.substream_ids.clone(),
                channels,
                decoders,
                queues,
                dmx_cursor: ParamCursor::default(),
                recon_cursor: ParamCursor::default(),
                reconstructor: None,
                gain_default: crate::params::q78_db_to_linear(
                    sub_element.element_mix_gain.default_mix_gain,
                ),
                gain_cursor: GainCursor::default(),
                sample_rate: 0,
            });
        }
        let param_index = build_param_index(sub_mix, &parsed.audio_elements)?;

        // Auto sample type: s32le when any codec carries more than 16
        // bits, else s16le.
        let sample_type = settings.sample_type.unwrap_or_else(|| {
            let deep = parsed.codec_configs.iter().any(|c| {
                use iamf_obu::descriptors::DecoderConfig;
                match &c.decoder_config {
                    DecoderConfig::Lpcm { sample_size, .. } => *sample_size > 16,
                    DecoderConfig::Flac {
                        bits_per_sample, ..
                    } => *bits_per_sample > 16,
                    _ => false,
                }
            });
            if deep {
                OutputSampleType::Int32LittleEndian
            } else {
                OutputSampleType::Int16LittleEndian
            }
        });
        let norm_gain = match settings.loudness_target_db {
            Some(target_db) => content_loudness_db(sub_mix, settings.layout)
                .map_or(1.0, |content_db| {
                    10f32.powf((target_db - content_db) / 20.0)
                }),
            None => 1.0,
        };
        Ok(StreamDecoder {
            slots,
            param_index,
            target: settings.layout,
            sample_type,
            permutation: output_permutation(settings.layout, settings.channel_ordering),
            settings,
            selected_mix_id: mix.mix_presentation_id,
            output_gain_default: crate::params::q78_db_to_linear(
                sub_mix.output_mix_gain.default_mix_gain,
            ),
            output_cursor: GainCursor::default(),
            norm_gain,
            limiter: None,
            pending: Vec::new(),
            frame_size,
            ended: false,
            parsed,
        })
    }

    /// Pushes bitstream bytes: whole or partial OBUs, as much or as little
    /// as the caller has. Decoded temporal units accumulate until pulled
    /// with [`StreamDecoder::get_output_temporal_unit`].
    pub fn decode(&mut self, data: &[u8]) -> Result<(), DecodeError> {
        self.pending.extend_from_slice(data);
        // The buffer is taken out of `self` for the loop so OBU payloads
        // can be handled as borrowed slices (no per-OBU copies) while the
        // handlers take `&mut self`.
        let pending = std::mem::take(&mut self.pending);
        let mut consumed = 0usize;
        let result = self.decode_pending(&pending, &mut consumed);
        self.pending = pending;
        self.pending.drain(..consumed);
        result
    }

    fn decode_pending(&mut self, pending: &[u8], consumed: &mut usize) -> Result<(), DecodeError> {
        loop {
            let mut reader = ByteReader::new(&pending[*consumed..]);
            match Obu::parse(&mut reader) {
                Ok(obu) => {
                    let advance = reader.position();
                    let frame = AudioFrame::from_obu(&obu)
                        .map_err(|e| DecodeError::CorruptPacket(e.to_string()))?;
                    if let Some(frame) = frame {
                        self.handle_frame(
                            frame.substream_id,
                            frame.data,
                            frame.num_samples_to_trim_at_start,
                            frame.num_samples_to_trim_at_end,
                        )?;
                    } else if obu.header.obu_type == ObuType::ParameterBlock {
                        self.handle_parameter_block(obu.payload)?;
                    } else if obu.header.obu_type == ObuType::TemporalDelimiter {
                        self.check_unit_alignment()?;
                    }
                    // Descriptor OBUs after configuration are redundant
                    // copies; ignored.
                    *consumed += advance;
                }
                Err(Error::UnexpectedEof { .. }) => return Ok(()),
                Err(e) => return Err(DecodeError::CorruptPacket(e.to_string())),
            }
        }
    }

    /// §3.9: a temporal delimiter sits on a temporal-unit boundary, so
    /// every substream of every element must have the same number of
    /// buffered frames. A mismatch means a frame was lost or duplicated.
    fn check_unit_alignment(&self) -> Result<(), DecodeError> {
        let mut depth = None;
        for slot in &self.slots {
            for q in &slot.queues {
                let d = q.len();
                if *depth.get_or_insert(d) != d {
                    return Err(DecodeError::CorruptPacket(
                        "temporal delimiter mid-unit: substreams have unequal frame counts".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn handle_frame(
        &mut self,
        substream_id: u32,
        data: &[u8],
        trim_start: u32,
        trim_end: u32,
    ) -> Result<(), DecodeError> {
        for slot in &mut self.slots {
            let Some(index) = slot.substream_ids.iter().position(|&id| id == substream_id) else {
                continue;
            };
            let mut out = DecodedFrame::default();
            slot.decoders[index].decode(data, &mut out)?;
            slot.sample_rate = out.sample_rate;
            slot.queues[index].push_back(FramePcm {
                samples: out.samples,
                trim_start,
                trim_end,
            });
            return Ok(());
        }
        Ok(())
    }

    fn handle_parameter_block(&mut self, payload: &[u8]) -> Result<(), DecodeError> {
        let id = ParameterBlock::peek_parameter_id(payload)
            .map_err(|e| DecodeError::CorruptPacket(e.to_string()))?;
        let sample_rate = self.sample_rate();
        // Split borrows: the index is only read while slots/cursors are
        // updated, so no target list needs cloning.
        let StreamDecoder {
            param_index,
            slots,
            output_cursor,
            ..
        } = self;
        let Some(targets) = param_index.get(&id) else {
            return Ok(());
        };
        let corrupt = |e: Error| DecodeError::CorruptPacket(e.to_string());
        // libiamf scales parameter durations to the sample clock by
        // (rate + 0.1) / parameter_rate; before the first decoded frame of
        // an unknown-rate codec the rates are assumed equal.
        let ratio = |parameter_rate: u32| {
            if sample_rate == 0 {
                1.0
            } else {
                (f64::from(sample_rate) + 0.1) / f64::from(parameter_rate.max(1))
            }
        };
        for (slot_index, kind, definition) in targets {
            let scale = ratio(definition.parameter_rate);
            match kind {
                ParamKind::Demixing => {
                    let block = ParameterBlock::parse(payload, definition, &ParamContext::Demixing)
                        .map_err(corrupt)?;
                    for sb in &block.subblocks {
                        if let SubblockData::Demixing { dmixp_mode } = &sb.data {
                            slots[*slot_index]
                                .dmx_cursor
                                .push(*dmixp_mode, (f64::from(sb.duration) * scale) as usize);
                        }
                    }
                }
                ParamKind::ReconGain => {
                    let block = {
                        let AudioElementConfig::ChannelBased { layers } =
                            &slots[*slot_index].element.config
                        else {
                            continue;
                        };
                        ParameterBlock::parse(payload, definition, &ParamContext::ReconGain(layers))
                            .map_err(corrupt)?
                    };
                    for sb in block.subblocks {
                        if let SubblockData::ReconGain(gains) = sb.data {
                            slots[*slot_index]
                                .recon_cursor
                                .push(gains, (f64::from(sb.duration) * scale) as usize);
                        }
                    }
                }
                ParamKind::ElementMixGain | ParamKind::OutputMixGain => {
                    let block = ParameterBlock::parse(payload, definition, &ParamContext::MixGain)
                        .map_err(corrupt)?;
                    let cursor = match kind {
                        ParamKind::ElementMixGain => &mut slots[*slot_index].gain_cursor,
                        _ => &mut *output_cursor,
                    };
                    for sb in &block.subblocks {
                        if let SubblockData::MixGain(anim) = &sb.data {
                            cursor.push(anim, (f64::from(sb.duration) * scale) as usize);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Whether a complete temporal unit is decoded and ready to pull.
    pub fn is_temporal_unit_available(&self) -> bool {
        !self.slots.is_empty() && self.slots.iter().all(SlotState::unit_ready)
    }

    /// Pops and renders one temporal unit as interleaved little-endian PCM
    /// bytes. `None` when no unit is available.
    pub fn get_output_temporal_unit(&mut self) -> Result<Option<Vec<u8>>, DecodeError> {
        if !self.is_temporal_unit_available() {
            return Ok(None);
        }
        let target_matrix = self.target.matrix_layout();
        let out_channels = self.num_output_channels();
        let mut mixed: Vec<Vec<f32>> = vec![Vec::new(); out_channels];
        let mut trim: Option<(u32, u32)> = None;
        let mut unit_len: Option<usize> = None;

        for slot in &mut self.slots {
            let frames: Vec<FramePcm> = slot
                .queues
                .iter_mut()
                .map(|q| q.pop_front().expect("unit_ready checked"))
                .collect();
            let frame_len = frames[0].samples.len() / usize::from(slot.channels[0].max(1));
            // §3.9: trimming and frame duration are per temporal unit, so
            // every frame of the unit must agree.
            for frame in &frames {
                if (frame.trim_start, frame.trim_end) != (frames[0].trim_start, frames[0].trim_end)
                {
                    return Err(DecodeError::CorruptPacket(
                        "audio frames of one temporal unit disagree on trimming".into(),
                    ));
                }
            }
            if *unit_len.get_or_insert(frame_len) != frame_len {
                return Err(DecodeError::CorruptPacket(
                    "temporal unit frame lengths differ across elements".into(),
                ));
            }
            match trim {
                None => trim = Some((frames[0].trim_start, frames[0].trim_end)),
                Some(t) if t != (frames[0].trim_start, frames[0].trim_end) => {
                    return Err(DecodeError::CorruptPacket(
                        "audio frames of one temporal unit disagree on trimming".into(),
                    ));
                }
                Some(_) => {}
            }
            let dmx_mode = slot.dmx_cursor.take_for_unit(frame_len);
            let recon = slot.recon_cursor.take_for_unit(frame_len);

            let mut planes = Vec::new();
            for (frame, &ch) in frames.iter().zip(&slot.channels) {
                planes.extend(deinterleave(&frame.samples, usize::from(ch.max(1))));
            }

            // Binaural input is already headphone audio: never HRTF it.
            let hrtf = cfg!(feature = "binaural")
                && self.target == SoundSystem::Binaural
                && slot.headphones_rendering_mode == 1
                && !is_binaural_input(&slot.element.config);
            // Not if-let-else: the ambisonics arm is a peer case, not a
            // fallback.
            #[allow(clippy::single_match_else)]
            let rendered = match &slot.element.config {
                AudioElementConfig::ChannelBased { layers } => {
                    if slot.reconstructor.is_none() {
                        let mut rec =
                            ChannelReconstructor::with_layer_selection(layers, self.target, hrtf)?;
                        for param in &slot.element.params {
                            if let ElementParam::Demixing {
                                default_demixing_mode,
                                default_weight_index,
                                ..
                            } = param
                            {
                                rec.set_default_demixing(
                                    *default_demixing_mode,
                                    *default_weight_index,
                                )?;
                            }
                        }
                        slot.reconstructor = Some(rec);
                    }
                    let rec = slot.reconstructor.as_mut().unwrap();
                    if let Some(mode) = dmx_mode {
                        rec.set_demixing_mode(mode)?;
                    }
                    if let Some(recon) = &recon {
                        rec.set_recon_gains(recon);
                    }
                    let planar = rec.process_frame(&planes)?;
                    #[cfg(feature = "binaural")]
                    let rendered = if rec.is_binaural_input() {
                        // Passthrough (target is binaural or stereo).
                        planar
                    } else if hrtf {
                        let layout = rec.layout();
                        binauralize_unit(
                            &mut slot.binaural,
                            crate::binaural::BinauralInput::Speakers {
                                loudspeaker_layout: layout,
                            },
                            &planar,
                            frame_len,
                            slot.sample_rate,
                        )?
                    } else {
                        let reconstructed = crate::reconstruct::Reconstructed::Channels {
                            matrix: rec.matrix(),
                            planar,
                        };
                        render(&reconstructed, target_matrix)?
                    };
                    #[cfg(not(feature = "binaural"))]
                    let rendered = if rec.is_binaural_input() {
                        planar
                    } else {
                        let reconstructed = crate::reconstruct::Reconstructed::Channels {
                            matrix: rec.matrix(),
                            planar,
                        };
                        render(&reconstructed, target_matrix)?
                    };
                    rendered
                }
                _ => {
                    let reconstructed = ambisonics_from_planes(&slot.element.config, planes)?;
                    #[cfg(feature = "binaural")]
                    let rendered = if hrtf {
                        let hoa = reconstructed.planar();
                        let order = crate::reconstruct::hoa_order_index(hoa.len());
                        binauralize_unit(
                            &mut slot.binaural,
                            crate::binaural::BinauralInput::Hoa { order },
                            hoa,
                            frame_len,
                            slot.sample_rate,
                        )?
                    } else {
                        render(&reconstructed, target_matrix)?
                    };
                    #[cfg(not(feature = "binaural"))]
                    let rendered = render(&reconstructed, target_matrix)?;
                    rendered
                }
            };

            // Per-sample element mix gain over the untrimmed unit.
            let gains: Vec<f32> = (0..frame_len)
                .map(|_| slot.gain_cursor.next(slot.gain_default))
                .collect();
            for (mix_plane, rendered_plane) in mixed.iter_mut().zip(&rendered) {
                if mix_plane.len() < rendered_plane.len() {
                    mix_plane.resize(rendered_plane.len(), 0.0);
                }
                for ((o, &s), &g) in mix_plane.iter_mut().zip(rendered_plane).zip(&gains) {
                    *o += g * s;
                }
            }
        }

        // Output mix gain, then trimming, then loudness normalization and
        // peak limiting on the f32 signal, then interleave + quantize.
        let unit_len = unit_len.unwrap_or(0);
        let trim = trim.unwrap_or((0, 0));
        let out_gains: Vec<f32> = (0..unit_len)
            .map(|_| self.output_cursor.next(self.output_gain_default))
            .collect();
        let start = if self.settings.trimming.trim_beginning {
            (trim.0 as usize).min(unit_len)
        } else {
            0
        };
        let end = if self.settings.trimming.trim_end {
            (trim.1 as usize).min(unit_len - start)
        } else {
            0
        };
        let kept = unit_len - start - end;
        let mut samples = Vec::with_capacity(kept * out_channels);
        for (t, &gain) in out_gains
            .iter()
            .enumerate()
            .take(unit_len - end)
            .skip(start)
        {
            for &source in &self.permutation {
                let sample = mixed
                    .get(source)
                    .and_then(|p| p.get(t))
                    .copied()
                    .unwrap_or(0.0)
                    * gain;
                samples.push(sample * self.norm_gain);
            }
        }
        if self.settings.enable_limiter {
            if self.limiter.is_none() {
                self.limiter = Some(PeakLimiter::new(
                    LIMITER_THRESHOLD_DB,
                    self.sample_rate().max(1),
                    out_channels,
                    LIMITER_LOOKAHEAD,
                ));
            }
            // Per-unit limiting: the look-ahead works within the unit and
            // gain state carries across units (see post::PeakLimiter).
            self.limiter
                .as_mut()
                .expect("created above")
                .process_in_place(&mut samples);
        }
        let mut bytes = Vec::with_capacity(samples.len() * self.sample_type.bytes_per_sample());
        for &sample in &samples {
            match self.sample_type {
                OutputSampleType::Int16LittleEndian => {
                    bytes.extend(crate::post::quantize_s16(sample).to_le_bytes());
                }
                OutputSampleType::Int32LittleEndian => {
                    bytes.extend(crate::post::quantize_s32(sample).to_le_bytes());
                }
            }
        }
        Ok(Some(bytes))
    }

    /// Number of output audio channels rendered for the selected layout.
    pub fn num_output_channels(&self) -> usize {
        self.target.channels()
    }

    /// Output sampling rate in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.slots
            .iter()
            .map(|s| s.sample_rate)
            .find(|&r| r != 0)
            .unwrap_or_else(|| {
                self.slots
                    .first()
                    .map_or(0, |s| match &s.codec_config.decoder_config {
                        iamf_obu::descriptors::DecoderConfig::Opus { .. } => 48000,
                        iamf_obu::descriptors::DecoderConfig::Lpcm { sample_rate, .. }
                        | iamf_obu::descriptors::DecoderConfig::Flac { sample_rate, .. } => {
                            *sample_rate
                        }
                        _ => 0,
                    })
            })
    }

    /// Frame duration / number of samples per channel per temporal unit.
    pub fn frame_size(&self) -> u32 {
        self.frame_size
    }

    /// PCM sample encoding format of pulled audio data.
    pub fn sample_type(&self) -> OutputSampleType {
        self.sample_type
    }

    /// The mix presentation actually selected, and the output layout it is
    /// rendered to (iamf-tools `GetOutputMix` / `SelectedMix`).
    pub fn selected_mix(&self) -> (u32, SoundSystem) {
        (self.selected_mix_id, self.target)
    }

    /// Marks end of stream. Our pipeline holds no look-ahead, so any
    /// complete buffered units remain pullable and nothing else changes.
    pub fn signal_end_of_decoding(&mut self) {
        self.ended = true;
    }

    /// Returns true if end-of-stream has been signaled.
    pub fn is_ended(&self) -> bool {
        self.ended
    }

    /// Drops buffered audio and parameter state (seek/discontinuity).
    /// Codec decoders and demixer state are reset; the configuration is
    /// kept.
    pub fn reset(&mut self) {
        self.pending.clear();
        self.ended = false;
        self.output_cursor = GainCursor::default();
        self.limiter = None;
        for slot in &mut self.slots {
            for q in &mut slot.queues {
                q.clear();
            }
            slot.dmx_cursor.clear();
            slot.recon_cursor.clear();
            slot.reconstructor = None;
            #[cfg(feature = "binaural")]
            {
                slot.binaural = None;
            }
            slot.gain_cursor = GainCursor::default();
            for dec in &mut slot.decoders {
                dec.reset();
            }
        }
    }

    /// Reconfigures for a different mix presentation and/or output layout
    /// without reparsing descriptors (iamf-tools `ResetWithNewMix`). Codec
    /// decoders of audio elements shared between the old and new mix are
    /// reset and reused instead of recreated. Buffered audio and parameter
    /// state are dropped, like [`StreamDecoder::reset`].
    ///
    /// On error the decoder is left unconfigured (it accepts data but
    /// produces nothing) and should be reconfigured or destroyed.
    pub fn reset_with_new_mix(
        &mut self,
        selection: MixSelection,
        layout: Option<SoundSystem>,
        factory: &dyn CodecFactory,
    ) -> Result<(u32, SoundSystem), DecodeError> {
        let mut settings = self.settings;
        settings.mix_selection = selection;
        if let Some(layout) = layout {
            settings.layout = layout;
        }
        let mut reuse: Vec<(u32, Vec<Box<dyn SubstreamDecoder>>)> = self
            .slots
            .drain(..)
            .map(|s| (s.element.audio_element_id, s.decoders))
            .collect();
        self.param_index.clear();
        self.pending.clear();
        match Self::from_parsed(
            std::mem::take(&mut self.parsed),
            settings,
            factory,
            &mut reuse,
        ) {
            Ok(next) => {
                *self = next;
                Ok(self.selected_mix())
            }
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iamf_obu::descriptors::{
        ChannelAudioLayer, Layout, LoudnessInfo, MixGainParam, MixPresentation, ParamDefinition,
        SubMix, SubMixElement,
    };

    /// §3.8.2 headphones_rendering_mode: binaural (world-locked).
    const BINAURAL_MODE: u8 = 1;

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

    fn ss(sound_system: u8) -> Layout {
        Layout::LoudspeakersSsConvention { sound_system }
    }

    /// A sub-mix of `(audio_element_id, headphones_rendering_mode)`
    /// entries, declaring `layouts`.
    fn sub_mix(elements: &[(u32, u8)], layouts: &[Layout]) -> SubMix {
        SubMix {
            elements: elements
                .iter()
                .map(
                    |&(audio_element_id, headphones_rendering_mode)| SubMixElement {
                        audio_element_id,
                        localized_annotations: vec![],
                        headphones_rendering_mode,
                        element_mix_gain: gain(),
                    },
                )
                .collect(),
            output_mix_gain: gain(),
            layouts: layouts
                .iter()
                .map(|&layout| {
                    (
                        layout,
                        LoudnessInfo {
                            info_type: 0,
                            integrated_loudness: 0,
                            digital_peak: 0,
                            true_peak: None,
                            anchored_loudness: vec![],
                        },
                    )
                })
                .collect(),
        }
    }

    fn mix_with(id: u32, sub_mixes: Vec<SubMix>) -> MixPresentation {
        MixPresentation {
            mix_presentation_id: id,
            annotation_languages: vec![],
            localized_annotations: vec![],
            sub_mixes,
            tags: vec![],
        }
    }

    /// A mix with no audio elements declaring one sound system layout.
    fn mix(id: u32, sound_system: u8) -> MixPresentation {
        mix_with(id, vec![sub_mix(&[], &[ss(sound_system)])])
    }

    /// A scalable channel-based element with the given layer
    /// loudspeaker_layouts, lowest first.
    fn channel_element(id: u32, layouts: &[u8]) -> AudioElement {
        AudioElement {
            audio_element_id: id,
            codec_config_id: 0,
            substream_ids: vec![],
            params: vec![],
            config: AudioElementConfig::ChannelBased {
                layers: layouts
                    .iter()
                    .map(|&loudspeaker_layout| ChannelAudioLayer {
                        loudspeaker_layout,
                        substream_count: 1,
                        coupled_substream_count: 1,
                        recon_gain_is_present: false,
                        output_gain: None,
                        expanded_loudspeaker_layout: None,
                    })
                    .collect(),
            },
        }
    }

    fn foa_element(id: u32) -> AudioElement {
        AudioElement {
            audio_element_id: id,
            codec_config_id: 0,
            substream_ids: vec![],
            params: vec![],
            config: AudioElementConfig::AmbisonicsMono {
                output_channel_count: 4,
                substream_count: 4,
                channel_mapping: vec![0, 1, 2, 3],
            },
        }
    }

    /// Auto selection over all-supported mixes, as a mix_presentation_id.
    fn auto_id(mixes: &[MixPresentation], elements: &[AudioElement], target: SoundSystem) -> u32 {
        let supported = vec![true; mixes.len()];
        let index =
            select_mix_index(mixes, elements, &supported, MixSelection::Auto, target).unwrap();
        mixes[index].mix_presentation_id
    }

    const STEREO_ID: u32 = 1234;
    const BINAURAL_ID: u32 = 2222;
    const FOA_ID: u32 = 3333;
    const PREFERRED: u32 = 9999;

    fn elements() -> Vec<AudioElement> {
        vec![
            channel_element(STEREO_ID, &[LOUDSPEAKER_LAYOUT_STEREO]),
            channel_element(BINAURAL_ID, &[LOUDSPEAKER_LAYOUT_BINAURAL]),
            foa_element(FOA_ID),
        ]
    }

    #[test]
    fn mix_selection_modes() {
        let mixes = [mix(10, 0), mix(20, 9)];
        let all = [true, true];
        // Auto prefers the mix declaring the requested layout.
        assert_eq!(
            select_mix_index(&mixes, &[], &all, MixSelection::Auto, SoundSystem::J).unwrap(),
            1
        );
        // Auto falls back to the first when nothing matches.
        assert_eq!(
            select_mix_index(&mixes, &[], &all, MixSelection::Auto, SoundSystem::H).unwrap(),
            0
        );
        // Binaural playback with neither a binaural element nor a binaural
        // layout falls back to the first mix.
        assert_eq!(
            select_mix_index(&mixes, &[], &all, MixSelection::Auto, SoundSystem::Binaural).unwrap(),
            0
        );
        assert_eq!(
            select_mix_index(&mixes, &[], &all, MixSelection::ById(20), SoundSystem::A).unwrap(),
            1
        );
        // Unknown id falls back to automatic selection (iamf-tools
        // RequestedMix semantics).
        assert_eq!(
            select_mix_index(&mixes, &[], &all, MixSelection::ById(99), SoundSystem::A).unwrap(),
            0
        );
        assert_eq!(
            select_mix_index(&mixes, &[], &all, MixSelection::ByIndex(1), SoundSystem::A).unwrap(),
            1
        );
        assert!(
            select_mix_index(&mixes, &[], &all, MixSelection::ByIndex(2), SoundSystem::A).is_err()
        );
    }

    /// Clause 3: nothing matches the stereo clauses, take the first.
    #[test]
    fn stereo_without_creator_preferred_mix_takes_first() {
        let mixes = [mix(PREFERRED, 4), mix(2, 3)];
        assert_eq!(auto_id(&mixes, &elements(), SoundSystem::A), PREFERRED);
    }

    /// 2.2.1: one stereo layout, one stereo element, stereo rendering.
    #[test]
    fn stereo_selects_creator_preferred_mix() {
        let preferred = mix_with(PREFERRED, vec![sub_mix(&[(STEREO_ID, 0)], &[ss(0)])]);
        // Over a non-stereo mix.
        assert_eq!(
            auto_id(&[mix(1, 4), preferred.clone()], &elements(), SoundSystem::A),
            PREFERRED
        );
        // Over an earlier mix that merely declares stereo (the pre-§7.4.1
        // rule picked this one).
        assert_eq!(
            auto_id(&[mix(1, 0), preferred.clone()], &elements(), SoundSystem::A),
            PREFERRED
        );
        // A multi-layer element qualifies by its last (highest) layer.
        let layered = [channel_element(STEREO_ID, &[0, LOUDSPEAKER_LAYOUT_STEREO])];
        assert_eq!(
            auto_id(&[mix(1, 0), preferred], &layered, SoundSystem::A),
            PREFERRED
        );
    }

    /// 2.2.1: the first sub-mix must declare exactly one layout.
    #[test]
    fn stereo_prefers_mix_with_one_stereo_layout() {
        let mixes = [
            mix_with(1, vec![sub_mix(&[(STEREO_ID, 0)], &[ss(0), ss(9)])]),
            mix_with(PREFERRED, vec![sub_mix(&[(STEREO_ID, 0)], &[ss(0)])]),
        ];
        assert_eq!(auto_id(&mixes, &elements(), SoundSystem::A), PREFERRED);
    }

    /// 2.2.1: exactly one audio element, and it must be stereo.
    #[test]
    fn stereo_prefers_one_stereo_element() {
        let mixes = [
            mix_with(1, vec![sub_mix(&[(STEREO_ID, 0), (FOA_ID, 0)], &[ss(0)])]),
            mix_with(2, vec![sub_mix(&[(FOA_ID, 0)], &[ss(0)])]),
            mix_with(PREFERRED, vec![sub_mix(&[(STEREO_ID, 0)], &[ss(0)])]),
        ];
        assert_eq!(auto_id(&mixes, &elements(), SoundSystem::A), PREFERRED);
        // A stereo base layer under a 5.1 top layer is not a stereo element.
        let layered = [channel_element(STEREO_ID, &[LOUDSPEAKER_LAYOUT_STEREO, 2])];
        assert_eq!(auto_id(&mixes, &layered, SoundSystem::A), 1);
    }

    /// 2.2.1 prefers headphones_rendering_mode stereo over binaural.
    #[test]
    fn stereo_prefers_stereo_rendering_mode() {
        let mixes = [
            mix_with(1, vec![sub_mix(&[(STEREO_ID, BINAURAL_MODE)], &[ss(0)])]),
            mix_with(PREFERRED, vec![sub_mix(&[(STEREO_ID, 0)], &[ss(0)])]),
        ];
        assert_eq!(auto_id(&mixes, &elements(), SoundSystem::A), PREFERRED);
    }

    /// 2.2.2: without a stereo-rendering candidate, relax that constraint.
    #[test]
    fn stereo_falls_back_to_any_rendering_mode() {
        let fallback = mix_with(
            PREFERRED,
            vec![sub_mix(&[(STEREO_ID, BINAURAL_MODE)], &[ss(0)])],
        );
        assert_eq!(
            auto_id(&[mix(1, 4), fallback.clone()], &elements(), SoundSystem::A),
            PREFERRED
        );
        assert_eq!(
            auto_id(&[mix(1, 0), fallback], &elements(), SoundSystem::A),
            PREFERRED
        );
    }

    /// Mixes referencing unknown elements never match; fall back to first.
    #[test]
    fn stereo_bypasses_missing_audio_element() {
        let mixes = [
            mix_with(1, vec![sub_mix(&[(4321, 0)], &[ss(0)])]),
            mix(2, 0),
        ];
        assert_eq!(auto_id(&mixes, &elements(), SoundSystem::A), 1);
    }

    /// Only the first sub-mix is considered (an appended "system sound"
    /// sub-mix does not disqualify the mix, nor qualify it).
    #[test]
    fn stereo_is_based_on_first_sub_mix() {
        let stereo_sub_mix = || sub_mix(&[(STEREO_ID, 0)], &[ss(0)]);
        let mixes = [
            mix(1, 4),
            mix_with(PREFERRED, vec![stereo_sub_mix(), stereo_sub_mix()]),
        ];
        assert_eq!(auto_id(&mixes, &elements(), SoundSystem::A), PREFERRED);
        let second_only = [
            mix(1, 4),
            mix_with(2, vec![sub_mix(&[(FOA_ID, 0)], &[ss(4)]), stereo_sub_mix()]),
        ];
        assert_eq!(auto_id(&second_only, &elements(), SoundSystem::A), 1);
    }

    /// 2.1.1: exactly one audio element, authored binaural, wins over
    /// earlier stereo and binaural-layout mixes.
    #[test]
    fn binaural_selects_mix_with_one_binaural_element() {
        let mixes = [
            mix(1, 0),
            mix_with(2, vec![sub_mix(&[], &[Layout::Binaural])]),
            mix_with(
                PREFERRED,
                vec![sub_mix(&[(BINAURAL_ID, BINAURAL_MODE)], &[ss(0)])],
            ),
        ];
        assert_eq!(
            auto_id(&mixes, &elements(), SoundSystem::Binaural),
            PREFERRED
        );
        // Two elements disqualify the mix: 2.1.2 picks the binaural layout.
        let two = [
            mix(1, 0),
            mix_with(2, vec![sub_mix(&[], &[Layout::Binaural])]),
            mix_with(
                3,
                vec![sub_mix(&[(BINAURAL_ID, 0), (STEREO_ID, 0)], &[ss(0)])],
            ),
        ];
        assert_eq!(auto_id(&two, &elements(), SoundSystem::Binaural), 2);
    }

    /// 2.1.2: otherwise, the first mix declaring a binaural layout, in any
    /// sub-mix.
    #[test]
    fn binaural_selects_mix_by_loudness_layout() {
        let mixes = [
            mix(1, 0),
            mix_with(
                PREFERRED,
                vec![sub_mix(&[(FOA_ID, 0)], &[Layout::Binaural])],
            ),
        ];
        assert_eq!(
            auto_id(&mixes, &elements(), SoundSystem::Binaural),
            PREFERRED
        );
        let later_sub_mix = [
            mix(1, 0),
            mix_with(
                PREFERRED,
                vec![
                    sub_mix(&[(FOA_ID, 0)], &[ss(0)]),
                    sub_mix(&[(FOA_ID, 0)], &[Layout::Binaural]),
                ],
            ),
        ];
        assert_eq!(
            auto_id(&later_sub_mix, &elements(), SoundSystem::Binaural),
            PREFERRED
        );
    }

    /// Binaural output no longer prefers stereo-declaring mixes (upstream
    /// has no such clause; 2.1.3 is unimplemented): first mix wins.
    #[test]
    fn binaural_does_not_prefer_stereo_mixes() {
        let mixes = [mix(1, 9), mix(2, 0)];
        assert_eq!(auto_id(&mixes, &elements(), SoundSystem::Binaural), 1);
    }

    /// 2.3.1: other layouts pick the first mix declaring that exact
    /// layout in any sub-mix; stereo element preferences do not apply.
    #[test]
    fn other_layouts_select_first_declaring_mix() {
        let mixes = [
            mix_with(1, vec![sub_mix(&[(STEREO_ID, 0)], &[ss(0)])]),
            mix_with(
                2,
                vec![sub_mix(&[(FOA_ID, 0)], &[ss(0)]), sub_mix(&[], &[ss(9)])],
            ),
            mix(3, 9),
        ];
        assert_eq!(auto_id(&mixes, &elements(), SoundSystem::J), 2);
        assert_eq!(auto_id(&mixes, &elements(), SoundSystem::B), 1);
    }

    /// Clause 1: a supported requested id beats every layout clause.
    #[test]
    fn requested_id_takes_precedence() {
        let mixes = [
            mix(1, 4),
            mix_with(2, vec![sub_mix(&[(BINAURAL_ID, BINAURAL_MODE)], &[ss(0)])]),
            mix_with(3, vec![sub_mix(&[(STEREO_ID, 0)], &[ss(0)])]),
        ];
        let all = [true; 3];
        for target in [SoundSystem::A, SoundSystem::Binaural, SoundSystem::J] {
            assert_eq!(
                select_mix_index(&mixes, &elements(), &all, MixSelection::ById(1), target).unwrap(),
                0,
                "{target:?}"
            );
        }
        // An absent id falls through to the layout clauses.
        assert_eq!(
            select_mix_index(
                &mixes,
                &elements(),
                &all,
                MixSelection::ById(7),
                SoundSystem::Binaural
            )
            .unwrap(),
            1
        );
        assert_eq!(
            select_mix_index(
                &mixes,
                &elements(),
                &all,
                MixSelection::ById(7),
                SoundSystem::A
            )
            .unwrap(),
            2
        );
    }

    /// Unsupported mixes are skipped by every layout clause.
    #[test]
    fn layout_clauses_skip_unsupported_mixes() {
        let mixes = [
            mix(1, 4),
            mix_with(2, vec![sub_mix(&[(BINAURAL_ID, BINAURAL_MODE)], &[ss(0)])]),
            mix_with(3, vec![sub_mix(&[(STEREO_ID, 0)], &[ss(0)])]),
            mix_with(4, vec![sub_mix(&[], &[Layout::Binaural])]),
        ];
        let supported = [true, false, false, true];
        let pick = |target| {
            select_mix_index(&mixes, &elements(), &supported, MixSelection::Auto, target).unwrap()
        };
        assert_eq!(pick(SoundSystem::Binaural), 3);
        assert_eq!(pick(SoundSystem::A), 0);
    }

    #[test]
    fn output_permutations_are_bijections() {
        for system in 0..=14u8 {
            let target = SoundSystem::from_u8(system).unwrap();
            for ordering in [ChannelOrdering::Iamf, ChannelOrdering::Android] {
                let p = output_permutation(target, ordering);
                assert_eq!(p.len(), target.channels(), "{target:?} {ordering:?}");
                let mut seen = vec![false; p.len()];
                for &slot in &p {
                    assert!(slot < p.len(), "{target:?} {ordering:?}: index {slot}");
                    assert!(!seen[slot], "{target:?} {ordering:?}: duplicate {slot}");
                    seen[slot] = true;
                }
            }
        }
    }

    #[test]
    fn unsupported_mixes_are_skipped() {
        let mixes = [mix(10, 9), mix(20, 9)];
        let supported = [false, true];
        // Auto skips the unsupported mix even though it matches first.
        assert_eq!(
            select_mix_index(&mixes, &[], &supported, MixSelection::Auto, SoundSystem::J).unwrap(),
            1
        );
        // An id resolving to an unsupported mix falls back to auto.
        assert_eq!(
            select_mix_index(
                &mixes,
                &[],
                &supported,
                MixSelection::ById(10),
                SoundSystem::A
            )
            .unwrap(),
            1
        );
        // Explicit index to an unsupported mix is an error.
        assert!(matches!(
            select_mix_index(
                &mixes,
                &[],
                &supported,
                MixSelection::ByIndex(0),
                SoundSystem::A
            ),
            Err(DecodeError::UnsupportedProfile(_))
        ));
        // Nothing supported at all.
        assert!(matches!(
            select_mix_index(
                &mixes,
                &[],
                &[false, false],
                MixSelection::Auto,
                SoundSystem::A
            ),
            Err(DecodeError::UnsupportedProfile(_))
        ));
    }
}
