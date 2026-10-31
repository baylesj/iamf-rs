//! Iterative (streaming) decoder, shaped after the iamf-tools decoder API
//! that Chromium's `IamfAudioDecoder` consumes: configure from a
//! descriptor blob, push arbitrary byte chunks (whole or partial OBUs),
//! and pull decoded temporal units as interleaved little-endian PCM.

use std::collections::VecDeque;
use std::sync::Arc;

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
use crate::presentation::{Descriptors, EMPTY_SUB_MIX};
use crate::profile::ProfileSet;
use crate::reconstruct::{
    ChannelReconstructor, Reconstructed, ambisonics_from_planes, deinterleave,
};
use crate::render::render;
use crate::renderer::{
    ElementFrame, ElementInfo, ElementLayout, MixConfig, MixRenderer, MixRendererFactory,
};
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
    /// profile's limits are selectable. Defaults to the IAMF v1.1 profiles
    /// ([`ProfileSet::all_v1`]), as in iamf-tools.
    pub requested_profiles: ProfileSet,
    /// Loudness normalization target in dB (LKFS): applies a constant gain
    /// of `target - content` using the selected layout's `loudness_info`.
    /// `None` (the default) disables normalization, matching the iamf-tools
    /// decoder, which ignores loudness metadata.
    pub loudness_target_db: Option<f32>,
    /// libiamf-style look-ahead peak limiter at -1 dBFS. Off by default:
    /// the iamf-tools decoder (Chromium's reference) emits unlimited
    /// rendered PCM, while libiamf limits by default — integrators choose.
    /// The stream is limited as one signal: a unit becomes available once
    /// the following input covers the 5 ms look-ahead, and the last one
    /// after [`StreamDecoder::signal_end_of_decoding`].
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
            requested_profiles: ProfileSet::all_v1(),
            loudness_target_db: None,
            enable_limiter: false,
        }
    }
}

/// External render-step state (see [`crate::renderer`]): the factory, and
/// the renderer it created for the current configuration. The renderer is
/// created at the first temporal unit with audio, when the output sample
/// rate is known, and dropped on reset.
struct ExternalRenderer {
    factory: Arc<dyn MixRendererFactory>,
    renderer: Option<Box<dyn MixRenderer>>,
}

/// What an external renderer gets for one element of one temporal unit.
struct ExternalInput {
    info: ElementInfo,
    demixing_mode: Option<u8>,
    object_positions: Vec<crate::objects::PolarCoord>,
    planes: Vec<Vec<f32>>,
}

impl ExternalRenderer {
    /// Renders one temporal unit, creating the renderer first if needed.
    fn render_unit(
        &mut self,
        target: SoundSystem,
        sample_rate: u32,
        frame_size: u32,
        inputs: &[ExternalInput],
    ) -> Result<Vec<Vec<f32>>, DecodeError> {
        if self.renderer.is_none() {
            let elements: Vec<ElementInfo> = inputs.iter().map(|i| i.info.clone()).collect();
            let config = MixConfig {
                target,
                sample_rate,
                frame_size,
                elements: &elements,
            };
            self.renderer = Some(self.factory.create(&config)?);
        }
        let renderer = self.renderer.as_mut().expect("created above");
        let frames: Vec<ElementFrame<'_>> = inputs
            .iter()
            .map(|i| ElementFrame {
                audio_element_id: i.info.audio_element_id,
                demixing_mode: i.demixing_mode,
                object_positions: &i.object_positions,
                planes: &i.planes,
            })
            .collect();
        renderer.render(&frames)
    }
}

/// Length of the OBU at the start of `bytes` from its outer framing alone
/// (header byte, obu_size), when both are present and intact.
fn outer_obu_len(bytes: &[u8]) -> Option<usize> {
    let mut reader = ByteReader::new(bytes.get(1..)?);
    let size = reader.read_leb128().ok()? as usize;
    let len = 1 + reader.position() + size;
    (len <= bytes.len()).then_some(len)
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

/// Whether every sub mix of `mix` has an audio element to render (§3.7:
/// num_audio_elements SHALL NOT be 0).
fn has_audio_elements(mix: &iamf_obu::descriptors::MixPresentation) -> bool {
    mix.sub_mixes
        .iter()
        .all(|sub_mix| !sub_mix.elements.is_empty())
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
                        SoundSystem::from_bitstream_sound_system(*sound_system) == Some(other)
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
#[derive(Clone)]
struct GainCursor {
    default_db: i16,
    prev_end_db: i16,
    in_gap: bool,
    samples_pushed: usize,
    queue: VecDeque<(crate::params::LinearAnimation, usize, usize)>,
}

impl GainCursor {
    fn new(default_db: i16) -> Self {
        Self {
            default_db,
            prev_end_db: default_db,
            in_gap: false,
            samples_pushed: 0,
            queue: VecDeque::new(),
        }
    }

    fn clear(&mut self) {
        self.prev_end_db = self.default_db;
        self.in_gap = false;
        self.samples_pushed = 0;
        self.queue.clear();
    }

    fn advance_to(&mut self, target_samples: usize) {
        if target_samples > self.samples_pushed {
            let gap = target_samples - self.samples_pushed;
            self.prev_end_db = self.default_db;
            self.in_gap = false;
            let step = crate::params::MixGainAnimation::Step {
                start: self.default_db,
            };
            self.queue
                .push_back((crate::params::LinearAnimation::from(&step), gap, 0));
            self.samples_pushed = target_samples;
        }
    }

    fn push(&mut self, anim: &crate::params::MixGainAnimation, duration: usize) {
        if self.in_gap {
            self.prev_end_db = self.default_db;
            self.in_gap = false;
        }
        let (resolved, end_db) = anim.resolve(self.prev_end_db);
        self.prev_end_db = end_db;
        if duration > 0 {
            self.samples_pushed = self.samples_pushed.saturating_add(duration);
            self.queue
                .push_back((crate::params::LinearAnimation::from(&resolved), duration, 0));
        }
    }

    fn next(&mut self, default: f32) -> f32 {
        let Some((anim, duration, pos)) = self.queue.front_mut() else {
            self.in_gap = true;
            self.samples_pushed = self.samples_pushed.saturating_add(1);
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
    sub_mix_index: usize,
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
    /// Object position timeline (`Some` for object-based elements).
    position_cursor: Option<crate::objects::PositionCursor>,
    /// Builtin loudspeaker panner for object-based elements.
    object_renderer: Option<crate::objects::ObjectLoudspeakerRenderer>,
    reconstructor: Option<ChannelReconstructor>,
    /// §3.8.2: 0 = stereo fallback for headphones, 1/2 = HRTF binaural.
    headphones_rendering_mode: u8,
    binaural_filter_profile: iamf_obu::descriptors::BinauralFilterProfile,
    #[cfg(feature = "binaural")]
    binaural: Option<crate::binaural::BinauralRenderer>,
    gain_default: f32,
    gain_offset: f32,
    gain_cursor: GainCursor,
    sample_rate: u32,
    samples_decoded: usize,
}

impl SlotState {
    fn unit_ready(&self) -> bool {
        self.queues.iter().all(|q| !q.is_empty())
    }
}

/// Parameter cursors saved before rendering a unit
/// ([`StreamDecoder::get_output_temporal_unit`]).
struct CursorSnapshot {
    slots: Vec<SlotCursors>,
    sub_mixes: Vec<GainCursor>,
}

/// One slot's cursors: demixing, recon gain, element gain, position.
type SlotCursors = (
    ParamCursor<u8>,
    ParamCursor<ReconGainLayers>,
    GainCursor,
    Option<crate::objects::PositionCursor>,
);

struct SubMixState {
    output_gain_default: f32,
    output_cursor: GainCursor,
    /// Constant linear gain from loudness normalization (1.0 when off).
    norm_gain: f32,
    /// External render step replacing the builtin renderer for this sub-mix, if any.
    external: Option<ExternalRenderer>,
}

/// Feeds one temporal unit's planes through a slot's stateful binaural
/// renderer (created on first use with this unit's frame length).
#[cfg(feature = "binaural")]
fn binauralize_unit(
    renderer: &mut Option<crate::binaural::BinauralRenderer>,
    input: crate::binaural::BinauralInput,
    profile: crate::binaural::FilterProfile,
    planes: &[Vec<f32>],
    frame_len: usize,
    sample_rate: u32,
) -> Result<Vec<Vec<f32>>, DecodeError> {
    if renderer.is_none() {
        *renderer = Some(crate::binaural::BinauralRenderer::with_profile(
            input,
            profile,
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

/// Feeds one temporal unit's object planes and positions through a slot's
/// stateful binaural renderer.
#[cfg(feature = "binaural")]
fn binauralize_objects_unit(
    renderer: &mut Option<crate::binaural::BinauralRenderer>,
    num_objects: u8,
    profile: crate::binaural::FilterProfile,
    planes: &[Vec<f32>],
    positions: &[crate::objects::PolarCoord],
    frame_len: usize,
    sample_rate: u32,
) -> Result<Vec<Vec<f32>>, DecodeError> {
    if renderer.is_none() {
        *renderer = Some(crate::binaural::BinauralRenderer::with_profile(
            crate::binaural::BinauralInput::Objects {
                num_objects: usize::from(num_objects),
            },
            profile,
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
    let [l, right] = r.process_objects(&chunk, positions)?;
    Ok(vec![
        l[..frame_len.min(l.len())].to_vec(),
        right[..frame_len.min(right.len())].to_vec(),
    ])
}

/// The slot's channel reconstructor, created on first use with the
/// element's default demixing info.
fn slot_reconstructor<'a>(
    reconstructor: &'a mut Option<ChannelReconstructor>,
    element: &AudioElement,
    layers: &[iamf_obu::descriptors::ChannelAudioLayer],
    target: SoundSystem,
    force_highest: bool,
) -> Result<&'a mut ChannelReconstructor, DecodeError> {
    if reconstructor.is_none() {
        let mut rec = ChannelReconstructor::with_layer_selection(layers, target, force_highest)?;
        for param in &element.params {
            if let ElementParam::Demixing {
                default_demixing_mode,
                default_weight_index,
                ..
            } = param
            {
                rec.set_default_demixing(*default_demixing_mode, *default_weight_index)?;
            }
        }
        *reconstructor = Some(rec);
    }
    Ok(reconstructor.as_mut().expect("created above"))
}

/// The selected layout's `loudness_info` integrated loudness, in dB
/// (Q7.8 → dB). Falls back to the first measured layout when none matches.
fn content_loudness_db(sub_mix: &SubMix, target: SoundSystem) -> Option<f32> {
    use iamf_obu::descriptors::Layout;
    let matches = |layout: &Layout| match layout {
        Layout::LoudspeakersSsConvention { sound_system } => {
            SoundSystem::from_bitstream_sound_system(*sound_system) == Some(target)
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
    sub_mixes: Vec<SubMixState>,
    /// See [`ParamIndex`].
    param_index: ParamIndex,
    target: SoundSystem,
    sample_type: OutputSampleType,
    /// Output channel permutation: slot i of the interleaved output takes
    /// rendered channel `permutation[i]`.
    permutation: Vec<usize>,
    settings: StreamSettings,
    selected_mix_id: u32,
    /// Streaming peak limiter, created at the first pulled unit (it needs
    /// the resolved sample rate).
    limiter: Option<PeakLimiter>,
    /// Limiter output released but not yet returned (with the limiter on,
    /// a unit is returned once the following input completes its
    /// look-ahead).
    limited: VecDeque<f32>,
    /// Sample counts of rendered units not yet returned, oldest first.
    held: VecDeque<usize>,
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
        Self::from_parsed(parsed, settings, factory, &mut Vec::new(), None)
    }

    /// Like [`StreamDecoder::new_from_descriptors`], but renders through
    /// an external renderer from `renderer` instead of the builtin one
    /// (see [`crate::renderer`]). The factory is kept for the decoder's
    /// lifetime, including across [`StreamDecoder::reset_with_new_mix`].
    pub fn new_from_descriptors_with_renderer(
        descriptors: &[u8],
        settings: StreamSettings,
        factory: &dyn CodecFactory,
        renderer: Arc<dyn MixRendererFactory>,
    ) -> Result<Self, DecodeError> {
        let parsed = Descriptors::collect(descriptors)?;
        Self::from_parsed(parsed, settings, factory, &mut Vec::new(), Some(renderer))
    }

    /// Builds a configured decoder, harvesting matching codec decoders from
    /// `reuse` (element id → decoders) instead of creating new ones.
    #[allow(clippy::needless_pass_by_value)]
    fn from_parsed(
        parsed: Descriptors,
        settings: StreamSettings,
        factory: &dyn CodecFactory,
        reuse: &mut Vec<(u32, Vec<Box<dyn SubstreamDecoder>>)>,
        renderer: Option<Arc<dyn MixRendererFactory>>,
    ) -> Result<Self, DecodeError> {
        if parsed.mix_presentations.is_empty() {
            return Err(DecodeError::InvalidDescriptors(
                "no mix presentations".into(),
            ));
        }
        // §3.7 / iamf-tools ac2fff70b: a sub mix without audio elements is
        // never rendered. The profile filter marks such mixes unsupported,
        // so automatic (and by-id) selection skips them; an explicit index
        // to one, or a stream offering nothing else, is an error naming
        // the actual problem rather than a profile mismatch.
        let explicitly_empty = match settings.mix_selection {
            MixSelection::ByIndex(index) => parsed
                .mix_presentations
                .get(index)
                .is_some_and(|mix| !has_audio_elements(mix)),
            _ => false,
        };
        if explicitly_empty || !parsed.mix_presentations.iter().any(has_audio_elements) {
            return Err(DecodeError::InvalidDescriptors(EMPTY_SUB_MIX.into()));
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
        if mix.sub_mixes.is_empty() || mix.sub_mixes.len() > 2 {
            // Guaranteed by the profile filter; kept as a defensive check.
            return Err(DecodeError::InvalidDescriptors(
                "IAMF requires 1 or 2 sub mixes per mix presentation".into(),
            ));
        }
        if mix.sub_mixes.iter().any(|sm| sm.elements.is_empty()) {
            // Likewise guaranteed by the profile filter.
            return Err(DecodeError::InvalidDescriptors(EMPTY_SUB_MIX.into()));
        }
        crate::element::check_unique_substreams(mix, &parsed.audio_elements)?;
        for sub_mix in &mix.sub_mixes {
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
                let channels = substream_channels(&element.config);
                if channels.is_empty() || channels.len() != element.substream_ids.len() {
                    return Err(DecodeError::InvalidDescriptors(
                        "substream count mismatch".into(),
                    ));
                }
                if matches!(element.config, AudioElementConfig::ObjectBased { .. })
                    && sub_element.position_params.is_empty()
                {
                    return Err(DecodeError::InvalidDescriptors(
                        "object-based element requires a position parameter definition".into(),
                    ));
                }
            }
        }

        let active_sub_mixes = mix.sub_mixes.as_slice();

        let mut slots = Vec::new();
        let mut sub_mixes = Vec::with_capacity(active_sub_mixes.len());
        let mut frame_size = 0u32;
        for (sm_idx, sub_mix) in active_sub_mixes.iter().enumerate() {
            for sub_element in &sub_mix.elements {
                let element = parsed
                    .audio_elements
                    .iter()
                    .find(|e| e.audio_element_id == sub_element.audio_element_id)
                    .expect("validated above");
                let codec_config = parsed
                    .codec_configs
                    .iter()
                    .find(|c| c.codec_config_id == element.codec_config_id)
                    .expect("validated above");
                frame_size = codec_config.num_samples_per_frame;
                let channels = substream_channels(&element.config);
                let position_cursor = match &element.config {
                    AudioElementConfig::ObjectBased { num_objects, .. } => {
                        let pos_def = sub_element
                            .position_params
                            .first()
                            .expect("validated above");
                        Some(crate::objects::PositionCursor::from_definition(
                            *num_objects,
                            pos_def,
                        )?)
                    }
                    _ => None,
                };
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
                let gain_offset = match sub_element.element_gain_offset {
                    Some(
                        iamf_obu::descriptors::ElementGainOffsetConfig::Value { offset }
                        | iamf_obu::descriptors::ElementGainOffsetConfig::Range {
                            default: offset,
                            ..
                        },
                    ) => crate::params::q78_db_to_linear(offset),
                    _ => 1.0,
                };
                slots.push(SlotState {
                    sub_mix_index: sm_idx,
                    element: element.clone(),
                    codec_config: codec_config.clone(),
                    headphones_rendering_mode: sub_element.headphones_rendering_mode,
                    binaural_filter_profile: sub_element.binaural_filter_profile,
                    #[cfg(feature = "binaural")]
                    binaural: None,
                    substream_ids: element.substream_ids.clone(),
                    channels,
                    decoders,
                    queues,
                    dmx_cursor: ParamCursor::default(),
                    recon_cursor: ParamCursor::default(),
                    position_cursor,
                    object_renderer: None,
                    reconstructor: None,
                    gain_default: crate::params::q78_db_to_linear(
                        sub_element.element_mix_gain.default_mix_gain,
                    ),
                    gain_offset,
                    gain_cursor: GainCursor::new(sub_element.element_mix_gain.default_mix_gain),
                    sample_rate: 0,
                    samples_decoded: 0,
                });
            }
            let norm_gain = match settings.loudness_target_db {
                Some(target_db) => content_loudness_db(sub_mix, settings.layout)
                    .map_or(1.0, |content_db| {
                        10f32.powf((target_db - content_db) / 20.0)
                    }),
                None => 1.0,
            };
            sub_mixes.push(SubMixState {
                output_gain_default: crate::params::q78_db_to_linear(
                    sub_mix.output_mix_gain.default_mix_gain,
                ),
                output_cursor: GainCursor::new(sub_mix.output_mix_gain.default_mix_gain),
                norm_gain,
                external: renderer.as_ref().map(|factory| ExternalRenderer {
                    factory: Arc::clone(factory),
                    renderer: None,
                }),
            });
        }
        let param_index = build_param_index(active_sub_mixes, &parsed.audio_elements)?;

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
        Ok(StreamDecoder {
            slots,
            sub_mixes,
            param_index,
            target: settings.layout,
            sample_type,
            permutation: output_permutation(settings.layout, settings.channel_ordering),
            settings,
            selected_mix_id: mix.mix_presentation_id,
            limiter: None,
            limited: VecDeque::new(),
            held: VecDeque::new(),
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

    /// Handles every complete OBU in `pending[*consumed..]`.
    ///
    /// An OBU that fails is still consumed, so the next call resumes after
    /// it instead of failing on it again. A malformed OBU is skipped by
    /// its outer framing (header and obu_size) when that is intact;
    /// otherwise the buffered input cannot be resynchronized and is
    /// dropped.
    fn decode_pending(&mut self, pending: &[u8], consumed: &mut usize) -> Result<(), DecodeError> {
        loop {
            let rest = &pending[*consumed..];
            let mut reader = ByteReader::new(rest);
            match Obu::parse(&mut reader) {
                Ok(obu) => {
                    *consumed += reader.position();
                    self.handle_obu(&obu)?;
                }
                Err(Error::UnexpectedEof { .. }) => return Ok(()),
                Err(e) => {
                    *consumed += outer_obu_len(rest).unwrap_or(rest.len());
                    return Err(DecodeError::CorruptPacket(e.to_string()));
                }
            }
        }
    }

    fn handle_obu(&mut self, obu: &Obu<'_>) -> Result<(), DecodeError> {
        let frame =
            AudioFrame::from_obu(obu).map_err(|e| DecodeError::CorruptPacket(e.to_string()))?;
        if let Some(frame) = frame {
            self.handle_frame(
                frame.substream_id,
                frame.data,
                frame.num_samples_to_trim_at_start,
                frame.num_samples_to_trim_at_end,
            )
        } else if obu.header.obu_type == ObuType::ParameterBlock {
            self.handle_parameter_block(obu.payload)
        } else if obu.header.obu_type == ObuType::TemporalDelimiter {
            self.check_unit_alignment()
        } else {
            // Descriptor OBUs after configuration are redundant copies;
            // ignored.
            Ok(())
        }
    }

    /// §3.9: a temporal delimiter sits on a temporal-unit boundary, so
    /// every substream of every element must have the same number of
    /// buffered frames. A mismatch means a frame was lost or duplicated.
    /// On a mismatch the incomplete unit is dropped, so decoding resumes
    /// aligned with the next unit.
    fn check_unit_alignment(&mut self) -> Result<(), DecodeError> {
        let mut depth = None;
        for slot in &self.slots {
            for q in &slot.queues {
                let d = q.len();
                if *depth.get_or_insert(d) != d {
                    self.drop_partial_unit();
                    return Err(DecodeError::CorruptPacket(
                        "temporal delimiter mid-unit: substreams have unequal frame counts; \
                         dropped the incomplete unit"
                            .into(),
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
        // §3.9: the OBUs of a temporal unit are contiguous, so a substream
        // is never more than one frame ahead of another. A second frame for
        // a substream whose unit is still incomplete means that unit lost
        // frames: drop it (bounding what an incomplete stream can buffer)
        // and report the loss after queuing this frame.
        let min_depth = self.min_queue_depth();
        let ahead = self.slots.iter().any(|slot| {
            slot.substream_ids
                .iter()
                .zip(&slot.queues)
                .any(|(&id, q)| id == substream_id && q.len() > min_depth)
        });
        let lost = if ahead {
            self.drop_partial_unit();
            Some(DecodeError::CorruptPacket(format!(
                "audio frame for substream {substream_id} began a new temporal unit before the \
                 previous one completed; dropped the incomplete unit"
            )))
        } else {
            None
        };
        // Deliver to every element use that carries this substream (an
        // element may appear in both sub-mixes).
        for slot in &mut self.slots {
            let Some(index) = slot.substream_ids.iter().position(|&id| id == substream_id) else {
                continue;
            };
            let mut out = DecodedFrame::default();
            slot.decoders[index].decode(data, &mut out)?;
            crate::element::check_decoded_frame(
                &out,
                slot.channels[index],
                slot.codec_config.num_samples_per_frame,
            )?;
            slot.sample_rate = out.sample_rate;
            if index == 0 {
                slot.samples_decoded += out.samples.len() / usize::from(out.channels.max(1));
            }
            slot.queues[index].push_back(FramePcm {
                samples: out.samples,
                trim_start,
                trim_end,
            });
        }
        lost.map_or(Ok(()), Err)
    }

    /// Fewest frames buffered on any substream: the number of complete
    /// temporal units.
    fn min_queue_depth(&self) -> usize {
        self.slots
            .iter()
            .flat_map(|slot| slot.queues.iter().map(VecDeque::len))
            .min()
            .unwrap_or(0)
    }

    /// Drops the frames of the incomplete temporal unit (those beyond the
    /// complete units), so that every substream is aligned again.
    fn drop_partial_unit(&mut self) {
        let complete = self.min_queue_depth();
        for slot in &mut self.slots {
            for (index, q) in slot.queues.iter_mut().enumerate() {
                while q.len() > complete {
                    let frame = q.pop_back().expect("longer than complete");
                    if index == 0 {
                        slot.samples_decoded = slot.samples_decoded.saturating_sub(
                            frame.samples.len() / usize::from(slot.channels[0].max(1)),
                        );
                    }
                }
            }
        }
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
            sub_mixes,
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
                    let target_samples = slots[*slot_index].samples_decoded;
                    slots[*slot_index].dmx_cursor.advance_to(target_samples);
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
                    let target_samples = slots[*slot_index].samples_decoded;
                    slots[*slot_index].recon_cursor.advance_to(target_samples);
                    for sb in block.subblocks {
                        if let SubblockData::ReconGain(gains) = sb.data {
                            slots[*slot_index]
                                .recon_cursor
                                .push(gains, (f64::from(sb.duration) * scale) as usize);
                        }
                    }
                }
                ParamKind::Position(param_type) => {
                    let block = ParameterBlock::parse(
                        payload,
                        definition,
                        &ParamContext::Position(*param_type),
                    )
                    .map_err(corrupt)?;
                    let target_samples = slots[*slot_index].samples_decoded;
                    if let Some(cursor) = &mut slots[*slot_index].position_cursor {
                        cursor.advance_to(target_samples);
                        for sb in &block.subblocks {
                            if let SubblockData::Position(pos) = &sb.data {
                                cursor.push(pos, (f64::from(sb.duration) * scale) as usize)?;
                            }
                        }
                    }
                }
                ParamKind::ElementMixGain | ParamKind::OutputMixGain => {
                    let block = ParameterBlock::parse(payload, definition, &ParamContext::MixGain)
                        .map_err(corrupt)?;
                    let target_samples = match kind {
                        ParamKind::ElementMixGain => slots[*slot_index].samples_decoded,
                        _ => slots.first().map_or(0, |s| s.samples_decoded),
                    };
                    let cursor = match kind {
                        ParamKind::ElementMixGain => &mut slots[*slot_index].gain_cursor,
                        _ => &mut sub_mixes[*slot_index].output_cursor,
                    };
                    cursor.advance_to(target_samples);
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
    ///
    /// With the limiter enabled, a unit is ready only once the input that
    /// follows it covers the limiter's look-ahead (or decoding ended), so
    /// that streaming output equals limiting the whole stream.
    pub fn is_temporal_unit_available(&self) -> bool {
        if !self.settings.enable_limiter {
            return self.frames_ready();
        }
        let future = self.queued_unit_samples();
        let Some(front) = self.held.front().or(future.first()).copied() else {
            return false;
        };
        let incoming: usize = future.iter().sum();
        let mut releasable = self.limited.len();
        if let Some(limiter) = &self.limiter {
            releasable += limiter.released_by_push(incoming);
            if self.ended {
                releasable += limiter.released_by_finish(incoming);
            }
        } else {
            let lookahead = LIMITER_LOOKAHEAD * self.num_output_channels();
            releasable += if self.ended {
                incoming
            } else {
                incoming.saturating_sub(lookahead)
            };
        }
        releasable >= front
    }

    /// Whether every substream has a frame for the next temporal unit.
    fn frames_ready(&self) -> bool {
        !self.slots.is_empty() && self.slots.iter().all(SlotState::unit_ready)
    }

    /// Output samples (after trimming) of each complete temporal unit
    /// still queued.
    fn queued_unit_samples(&self) -> Vec<usize> {
        let Some(first) = self.slots.first() else {
            return Vec::new();
        };
        let channels = usize::from(first.channels[0].max(1));
        let units = self.min_queue_depth();
        first.queues[0]
            .iter()
            .take(units)
            .map(|frame| {
                let len = frame.samples.len() / channels;
                let start = if self.settings.trimming.trim_beginning {
                    (frame.trim_start as usize).min(len)
                } else {
                    0
                };
                let end = if self.settings.trimming.trim_end {
                    (frame.trim_end as usize).min(len - start)
                } else {
                    0
                };
                (len - start - end) * self.num_output_channels()
            })
            .collect()
    }

    /// Pops and renders one temporal unit as interleaved little-endian PCM
    /// bytes. `None` when no unit is available.
    ///
    /// The unit is consumed even when this fails: every element skips it
    /// and its parameter timelines advance past it, so later units stay
    /// aligned across elements.
    pub fn get_output_temporal_unit(&mut self) -> Result<Option<Vec<u8>>, DecodeError> {
        if !self.settings.enable_limiter {
            return Ok(self
                .render_next_unit()?
                .map(|samples| self.quantize(&samples)));
        }
        // The limiter releases a unit's samples once the following input
        // covers its look-ahead: render ahead until the oldest held unit
        // is complete.
        loop {
            if let Some(&len) = self.held.front() {
                if self.limited.len() >= len {
                    self.held.pop_front();
                    let samples: Vec<f32> = self.limited.drain(..len).collect();
                    return Ok(Some(self.quantize(&samples)));
                }
            }
            if self.frames_ready() {
                let Some(samples) = self.render_next_unit()? else {
                    return Ok(None);
                };
                let channels = self.num_output_channels();
                let sample_rate = self.sample_rate().max(1);
                let limiter = self.limiter.get_or_insert_with(|| {
                    PeakLimiter::new(
                        LIMITER_THRESHOLD_DB,
                        sample_rate,
                        channels,
                        LIMITER_LOOKAHEAD,
                    )
                });
                self.limited.extend(limiter.push(&samples));
                self.held.push_back(samples.len());
                continue;
            }
            if self.ended && !self.held.is_empty() {
                if let Some(limiter) = &mut self.limiter {
                    let rest = limiter.finish();
                    if !rest.is_empty() {
                        self.limited.extend(rest);
                        continue;
                    }
                }
            }
            return Ok(None);
        }
    }

    fn quantize(&self, samples: &[f32]) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(samples.len() * self.sample_type.bytes_per_sample());
        for &sample in samples {
            match self.sample_type {
                OutputSampleType::Int16LittleEndian => {
                    bytes.extend(crate::post::quantize_s16(sample).to_le_bytes());
                }
                OutputSampleType::Int32LittleEndian => {
                    bytes.extend(crate::post::quantize_s32(sample).to_le_bytes());
                }
            }
        }
        bytes
    }

    /// Renders the next temporal unit to interleaved f32 samples (after
    /// trimming, before limiting); `None` when no unit is complete.
    fn render_next_unit(&mut self) -> Result<Option<Vec<f32>>, DecodeError> {
        if !self.frames_ready() {
            return Ok(None);
        }
        let snapshot = self.cursor_snapshot();
        let units: Vec<Vec<FramePcm>> = self
            .slots
            .iter_mut()
            .map(|slot| {
                slot.queues
                    .iter_mut()
                    .map(|q| q.pop_front().expect("unit available"))
                    .collect()
            })
            .collect();
        match self.render_popped_unit(units) {
            Ok(samples) => Ok(Some(samples)),
            Err(e) => {
                self.skip_unit(snapshot);
                Err(e)
            }
        }
    }

    /// Clones of every parameter cursor a unit advances.
    fn cursor_snapshot(&self) -> CursorSnapshot {
        CursorSnapshot {
            slots: self
                .slots
                .iter()
                .map(|slot| {
                    (
                        slot.dmx_cursor.clone(),
                        slot.recon_cursor.clone(),
                        slot.gain_cursor.clone(),
                        slot.position_cursor.clone(),
                    )
                })
                .collect(),
            sub_mixes: self
                .sub_mixes
                .iter()
                .map(|sm| sm.output_cursor.clone())
                .collect(),
        }
    }

    /// Restores `snapshot` and advances every cursor by one unit, as if
    /// the unit had been rendered.
    fn skip_unit(&mut self, snapshot: CursorSnapshot) {
        let frame_len = self.frame_size as usize;
        for (slot, (dmx, recon, gain, position)) in self.slots.iter_mut().zip(snapshot.slots) {
            slot.dmx_cursor = dmx;
            slot.recon_cursor = recon;
            slot.gain_cursor = gain;
            slot.position_cursor = position;
            slot.dmx_cursor.take_for_unit(frame_len);
            slot.recon_cursor.take_for_unit(frame_len);
            for _ in 0..frame_len {
                slot.gain_cursor.next(slot.gain_default);
            }
            if let Some(cursor) = &mut slot.position_cursor {
                cursor.take_for_unit(frame_len);
            }
        }
        for (sm, output) in self.sub_mixes.iter_mut().zip(snapshot.sub_mixes) {
            sm.output_cursor = output;
            for _ in 0..frame_len {
                sm.output_cursor.next(sm.output_gain_default);
            }
        }
    }

    /// Renders one unit whose frames were already popped from every slot.
    fn render_popped_unit(&mut self, units: Vec<Vec<FramePcm>>) -> Result<Vec<f32>, DecodeError> {
        let target_matrix = self.target.matrix_layout();
        let out_channels = self.num_output_channels();
        let num_sub_mixes = self.sub_mixes.len();
        let mut sub_mixed: Vec<Vec<Vec<f32>>> = vec![vec![Vec::new(); out_channels]; num_sub_mixes];
        let mut trim: Option<(u32, u32)> = None;
        let mut unit_len: Option<usize> = None;
        let external = self
            .sub_mixes
            .first()
            .is_some_and(|sm| sm.external.is_some());
        let mut external_inputs: Vec<Vec<ExternalInput>> =
            (0..num_sub_mixes).map(|_| Vec::new()).collect();

        for (slot, frames) in self.slots.iter_mut().zip(units) {
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

            // Per-sample element mix gain over the untrimmed unit.
            let gains: Vec<f32> = (0..frame_len)
                .map(|_| slot.gain_cursor.next(slot.gain_default) * slot.gain_offset)
                .collect();

            // Binaural input is already headphone audio: never HRTF it.
            let binaural_input = is_binaural_input(&slot.element.config);
            let is_object = matches!(slot.element.config, AudioElementConfig::ObjectBased { .. });
            let headphones = self.target == SoundSystem::Binaural && !binaural_input;
            let hrtf = cfg!(feature = "binaural")
                && headphones
                && (matches!(slot.headphones_rendering_mode, 1 | 2) || is_object);
            // Layer selection: the builtin renderer binauralizes the highest
            // layer for modes 1 and 2 (with the `binaural` feature; otherwise
            // it renders stereo). An external renderer binauralizes modes 1
            // (world-locked) and 2 (head-locked) itself, so it always gets
            // the highest layer for them, whatever the features.
            let force_highest = if external {
                headphones && (matches!(slot.headphones_rendering_mode, 1 | 2) || is_object)
            } else {
                hrtf
            };

            if let AudioElementConfig::ObjectBased { num_objects, .. } = &slot.element.config {
                let positions = slot
                    .position_cursor
                    .as_mut()
                    .map(|c| c.take_for_unit(frame_len))
                    .unwrap_or_default();
                if external {
                    for plane in &mut planes {
                        for (s, &g) in plane.iter_mut().zip(&gains) {
                            *s *= g;
                        }
                    }
                    external_inputs[slot.sub_mix_index].push(ExternalInput {
                        info: ElementInfo {
                            audio_element_id: slot.element.audio_element_id,
                            layout: ElementLayout::Objects {
                                num_objects: *num_objects,
                            },
                            headphones_rendering_mode: slot.headphones_rendering_mode,
                            binaural_filter_profile: slot.binaural_filter_profile,
                            default_demixing_mode: None,
                        },
                        demixing_mode: None,
                        object_positions: positions,
                        planes,
                    });
                    continue;
                }
                let rendered = if self.target == SoundSystem::Binaural {
                    #[cfg(feature = "binaural")]
                    {
                        binauralize_objects_unit(
                            &mut slot.binaural,
                            *num_objects,
                            crate::binaural::FilterProfile::from(slot.binaural_filter_profile),
                            &planes,
                            &positions,
                            frame_len,
                            slot.sample_rate,
                        )?
                    }
                    #[cfg(not(feature = "binaural"))]
                    {
                        if slot.object_renderer.is_none() {
                            slot.object_renderer =
                                Some(crate::objects::ObjectLoudspeakerRenderer::new(
                                    SoundSystem::A,
                                    usize::from(*num_objects),
                                )?);
                        }
                        slot.object_renderer
                            .as_mut()
                            .expect("created above")
                            .process_frame(&planes, &positions)
                    }
                } else {
                    if slot.object_renderer.is_none() {
                        slot.object_renderer =
                            Some(crate::objects::ObjectLoudspeakerRenderer::new(
                                self.target,
                                usize::from(*num_objects),
                            )?);
                    }
                    slot.object_renderer
                        .as_mut()
                        .expect("created above")
                        .process_frame(&planes, &positions)
                };
                let mix_planes = &mut sub_mixed[slot.sub_mix_index];
                for (mix_plane, rendered_plane) in mix_planes.iter_mut().zip(&rendered) {
                    if mix_plane.len() < rendered_plane.len() {
                        mix_plane.resize(rendered_plane.len(), 0.0);
                    }
                    for ((o, &s), &g) in mix_plane.iter_mut().zip(rendered_plane).zip(&gains) {
                        *o += g * s;
                    }
                }
                continue;
            }

            // Reconstruction (shared by both render paths): demixing/recon
            // gain for channel-based elements, ACN planes for scene-based
            // ones.
            let reconstructed = match &slot.element.config {
                AudioElementConfig::ChannelBased { layers } => {
                    let rec = slot_reconstructor(
                        &mut slot.reconstructor,
                        &slot.element,
                        layers,
                        self.target,
                        force_highest,
                    )?;
                    if let Some(mode) = dmx_mode {
                        rec.set_demixing_mode(mode)?;
                    }
                    if let Some(recon) = &recon {
                        rec.set_recon_gains(recon);
                    }
                    let planar = rec.process_frame(&planes)?;
                    Reconstructed::Channels {
                        matrix: rec.matrix(),
                        planar,
                    }
                }
                _ => ambisonics_from_planes(&slot.element.config, planes)?,
            };

            if external {
                // The external renderer mixes the whole sub-mix at once, so
                // the gain is applied to the element input (renderers are
                // linear, so this equals gaining the rendered element).
                let layout = match (&reconstructed, &slot.reconstructor) {
                    (Reconstructed::Channels { .. }, Some(rec)) => ElementLayout::Channels {
                        loudspeaker_layout: if binaural_input {
                            LOUDSPEAKER_LAYOUT_BINAURAL
                        } else {
                            rec.layout()
                        },
                    },
                    _ => ElementLayout::Ambisonics {
                        order: reconstructed.planar().len().isqrt().saturating_sub(1) as u8,
                    },
                };
                let default_demixing_mode = slot.element.params.iter().find_map(|p| match p {
                    ElementParam::Demixing {
                        default_demixing_mode,
                        ..
                    } => Some(*default_demixing_mode),
                    _ => None,
                });
                let mut planes = reconstructed.into_planar();
                for plane in &mut planes {
                    for (s, &g) in plane.iter_mut().zip(&gains) {
                        *s *= g;
                    }
                }
                external_inputs[slot.sub_mix_index].push(ExternalInput {
                    info: ElementInfo {
                        audio_element_id: slot.element.audio_element_id,
                        layout,
                        headphones_rendering_mode: slot.headphones_rendering_mode,
                        binaural_filter_profile: slot.binaural_filter_profile,
                        default_demixing_mode,
                    },
                    demixing_mode: dmx_mode,
                    object_positions: Vec::new(),
                    planes,
                });
                continue;
            }

            // Builtin renderer: binaural input passes through unchanged (the
            // target is binaural or stereo, see `layout::can_render_to`);
            // everything else uses the libiamf gain matrices, or the
            // obr-style binaural renderer for headphones_rendering_mode 1/2.
            #[cfg(feature = "binaural")]
            let rendered = if binaural_input {
                reconstructed.into_planar()
            } else if hrtf {
                let input = match &slot.reconstructor {
                    Some(rec) if matches!(reconstructed, Reconstructed::Channels { .. }) => {
                        crate::binaural::BinauralInput::Speakers {
                            loudspeaker_layout: rec.layout(),
                        }
                    }
                    _ => crate::binaural::BinauralInput::Hoa {
                        order: crate::reconstruct::hoa_order_index(reconstructed.planar().len()),
                    },
                };
                binauralize_unit(
                    &mut slot.binaural,
                    input,
                    crate::binaural::FilterProfile::from(slot.binaural_filter_profile),
                    reconstructed.planar(),
                    frame_len,
                    slot.sample_rate,
                )?
            } else {
                render(&reconstructed, target_matrix)?
            };
            #[cfg(not(feature = "binaural"))]
            let rendered = if binaural_input {
                reconstructed.into_planar()
            } else {
                render(&reconstructed, target_matrix)?
            };

            let mix_planes = &mut sub_mixed[slot.sub_mix_index];
            for (mix_plane, rendered_plane) in mix_planes.iter_mut().zip(&rendered) {
                if mix_plane.len() < rendered_plane.len() {
                    mix_plane.resize(rendered_plane.len(), 0.0);
                }
                for ((o, &s), &g) in mix_plane.iter_mut().zip(rendered_plane).zip(&gains) {
                    *o += g * s;
                }
            }
        }

        // A unit without samples has nothing to render; skipping it also
        // keeps an external renderer from being sized by an empty unit.
        if unit_len.unwrap_or(0) > 0 {
            let sample_rate = self.sample_rate();
            let frame_size = self.frame_size;
            let target = self.target;
            for (sm_idx, sm) in self.sub_mixes.iter_mut().enumerate() {
                if let Some(external) = &mut sm.external {
                    sub_mixed[sm_idx] = external.render_unit(
                        target,
                        sample_rate,
                        frame_size,
                        &external_inputs[sm_idx],
                    )?;
                    if sub_mixed[sm_idx].len() != out_channels {
                        return Err(DecodeError::UnsupportedRenderer(format!(
                            "external renderer returned {} channels, {target:?} has {out_channels}",
                            sub_mixed[sm_idx].len()
                        )));
                    }
                }
            }
        }

        // Output mix gain and loudness normalization per sub-mix, then sum
        // sub-mixes, then trimming and peak limiting on the f32 signal,
        // then interleave + quantize.
        let unit_len = unit_len.unwrap_or(0);
        let trim = trim.unwrap_or((0, 0));
        let mut mixed = vec![vec![0.0f32; unit_len]; out_channels];
        for (sm_idx, sm) in self.sub_mixes.iter_mut().enumerate() {
            let out_gains: Vec<f32> = (0..unit_len)
                .map(|_| sm.output_cursor.next(sm.output_gain_default) * sm.norm_gain)
                .collect();
            for (ch, plane) in sub_mixed[sm_idx].iter().enumerate() {
                if let Some(dst) = mixed.get_mut(ch) {
                    for ((d, &s), &g) in dst.iter_mut().zip(plane).zip(&out_gains) {
                        *d += s * g;
                    }
                }
            }
        }

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
        for t in start..(unit_len - end) {
            for &source in &self.permutation {
                let sample = mixed
                    .get(source)
                    .and_then(|p| p.get(t))
                    .copied()
                    .unwrap_or(0.0);
                samples.push(sample);
            }
        }
        Ok(samples)
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
                    .and_then(|s| crate::profile::output_sample_rate(&s.codec_config))
                    .unwrap_or(0)
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
        self.limiter = None;
        self.limited.clear();
        self.held.clear();
        for sm in &mut self.sub_mixes {
            sm.output_cursor.clear();
            if let Some(external) = &mut sm.external {
                external.renderer = None;
            }
        }
        for slot in &mut self.slots {
            for q in &mut slot.queues {
                q.clear();
            }
            slot.dmx_cursor.clear();
            slot.recon_cursor.clear();
            if let Some(pos) = &mut slot.position_cursor {
                pos.clear();
            }
            slot.object_renderer = None;
            slot.reconstructor = None;
            #[cfg(feature = "binaural")]
            {
                slot.binaural = None;
            }
            slot.gain_cursor.clear();
            slot.samples_decoded = 0;
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
        // The external renderer factory survives a failed reconfiguration
        // so that a later successful one keeps using it.
        let renderer = self
            .sub_mixes
            .first()
            .and_then(|sm| sm.external.as_ref())
            .map(|e| Arc::clone(&e.factory));
        // Clone rather than take the descriptors: on failure they must
        // stay, so that a later call with valid arguments can succeed.
        match Self::from_parsed(self.parsed.clone(), settings, factory, &mut reuse, renderer) {
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

    /// Regression: GainCursor counted pushed samples with `+=`, which
    /// overflowed (a panic with overflow checks) once scaled subblock
    /// durations summed past usize::MAX.
    #[test]
    fn gain_cursor_huge_durations_saturate() {
        let mut cursor = GainCursor::new(0);
        let step = crate::params::MixGainAnimation::Step { start: -256 };
        cursor.push(&step, usize::MAX);
        cursor.push(&step, usize::MAX);
        cursor.advance_to(usize::MAX);
        assert!(cursor.next(1.0).is_finite());
        let mut empty = GainCursor::new(0);
        empty.advance_to(usize::MAX);
        empty.queue.clear();
        assert_eq!(empty.next(1.0), 1.0);
    }
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
                        binaural_filter_profile:
                            iamf_obu::descriptors::BinauralFilterProfile::default(),
                        position_params: vec![],
                        element_gain_offset: None,
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
                            layout_extension: vec![],
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
            optional_fields: None,
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
