//! Iterative (streaming) decoder, shaped after the iamf-tools decoder API
//! that Chromium's `IamfAudioDecoder` consumes: configure from a
//! descriptor blob, push arbitrary byte chunks (whole or partial OBUs),
//! and pull decoded temporal units as interleaved little-endian PCM.

use std::collections::VecDeque;

use iamf_obu::descriptors::{AudioElement, AudioElementConfig, CodecConfig, ElementParam, SubMix};
use iamf_obu::{AudioFrame, ByteReader, Error, Obu, ObuType};

use crate::element::{FramePcm, substream_channels};
use crate::layout::SoundSystem;
use crate::params::{
    ParamContext, ParamCursor, ParamIndex, ParamKind, ParameterBlock, ReconGainLayers,
    SubblockData, build_param_index,
};
use crate::position::ObjectPosition;
use crate::post::{LIMITER_LOOKAHEAD, LIMITER_THRESHOLD_DB, PeakLimiter};
use crate::presentation::Descriptors;
use crate::profile::{ProfileSet, filter_profiles_for_mix};
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

impl TrimmingSettings {
    /// Samples to drop at the start and the end of a `len`-sample unit
    /// whose frames signal `trim` (start, end), as these settings apply it.
    fn window(self, trim: Option<(u32, u32)>, len: usize) -> (usize, usize) {
        let (trim_start, trim_end) = trim.unwrap_or((0, 0));
        let start = if self.trim_beginning {
            (trim_start as usize).min(len)
        } else {
            0
        };
        let end = if self.trim_end {
            (trim_end as usize).min(len - start)
        } else {
            0
        };
        (start, end)
    }
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
    /// IAMF v2.0 object-based elements: hand them out instead of rendering
    /// them. Each temporal unit's objects (PCM after the mix's gains and
    /// trimming, plus their position over the unit) are then available
    /// from [`StreamDecoder::take_objects`], and only the other elements
    /// are rendered into the output layout. Off by default: object
    /// rendering is not implemented, so without this flag mix
    /// presentations containing objects are not selectable (a v2 stream's
    /// v1.1 fallback mix, if any, is chosen instead).
    pub object_passthrough: bool,
    /// Samples between two object position points in passthrough mode.
    pub object_position_interval: u32,
}

impl StreamSettings {
    /// The profiles mix selection may use: without object passthrough,
    /// only those whose mixes this decoder can render (the v1.1 ones).
    fn selectable_profiles(&self) -> ProfileSet {
        if self.object_passthrough {
            self.requested_profiles
        } else {
            self.requested_profiles.intersection(ProfileSet::V1)
        }
    }
}

/// One object of an object-based element over one temporal unit, from
/// [`StreamDecoder::take_objects`] (object passthrough).
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedObject {
    /// The audio element it belongs to.
    pub audio_element_id: u32,
    /// Its index in the element: 0, or 1 for the second object of a
    /// two-object element.
    pub index: u8,
    /// Mono PCM after the element and output mix gains, the element gain
    /// offset, loudness normalization and the unit's trimming.
    pub samples: Vec<f32>,
    /// Where it is, at sample offsets into `samples` (ascending, the first
    /// at 0, then every `object_position_interval` samples).
    pub positions: Vec<(u32, ObjectPosition)>,
}

/// Mix presentation selection (iamf-tools `RequestedMix` shape).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum MixSelection {
    /// Prefer a mix presentation that declares a layout matching the
    /// requested output layout; fall back to the first supported.
    #[default]
    Auto,
    /// Select by mix_presentation_id. When no supported mix carries the id,
    /// selection proceeds as if unspecified (iamf-tools `RequestedMix`
    /// semantics).
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
            object_passthrough: false,
            object_position_interval: 256,
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

/// Resolves a mix selection against parsed descriptors. `supported[i]`
/// says whether mix i fits some requested profile (see
/// [`filter_profiles_for_mix`]); unsupported mixes are never selected.
pub(crate) fn select_mix_index(
    mixes: &[iamf_obu::descriptors::MixPresentation],
    supported: &[bool],
    selection: MixSelection,
    target: SoundSystem,
) -> Result<usize, DecodeError> {
    if let MixSelection::ByIndex(index) = selection {
        return match supported.get(index) {
            Some(true) => Ok(index),
            Some(false) => Err(DecodeError::UnsupportedProfile(format!(
                "mix presentation {index} exceeds the requested profiles"
            ))),
            None => Err(DecodeError::InvalidDescriptors(
                "no such mix presentation".into(),
            )),
        };
    }
    if let MixSelection::ById(id) = selection {
        // A missing or unsupported id falls back to automatic selection
        // (iamf-tools `RequestedMix`: "the decoder will behave as if it
        // was unspecified").
        if let Some(index) = mixes
            .iter()
            .position(|m| m.mix_presentation_id == id)
            .filter(|&i| supported[i])
        {
            return Ok(index);
        }
    }
    // Binaural playback matches mixes authored for stereo.
    let wanted = match target {
        SoundSystem::Binaural => SoundSystem::A,
        other => other,
    };
    let declares_target = |m: &iamf_obu::descriptors::MixPresentation| {
        m.sub_mixes.iter().any(|sm| {
            sm.layouts.iter().any(|(layout, _)| match layout {
                iamf_obu::descriptors::Layout::LoudspeakersSsConvention { sound_system } => {
                    SoundSystem::from_u8(*sound_system) == Some(wanted)
                }
                iamf_obu::descriptors::Layout::Binaural => target == SoundSystem::Binaural,
                iamf_obu::descriptors::Layout::Reserved { .. } => false,
            })
        })
    };
    mixes
        .iter()
        .enumerate()
        .position(|(i, m)| supported[i] && declares_target(m))
        .or_else(|| supported.iter().position(|&s| s))
        .ok_or_else(|| {
            DecodeError::UnsupportedProfile(
                "no mix presentation is supported by the requested profiles".into(),
            )
        })
}

/// The position timeline of an object-based element from the position
/// parameter its mix declares. A mix that declares none (which IAMF does
/// not allow) leaves its objects at the front, on the unit sphere.
fn object_position_cursor(
    sub_element: &iamf_obu::descriptors::SubMixElement,
    num_objects: u8,
) -> Result<crate::position::PositionCursor, DecodeError> {
    use iamf_obu::descriptors::{ParamDefinition, PositionKind, PositionParam};
    let param = sub_element
        .position
        .clone()
        .unwrap_or_else(|| PositionParam {
            base: ParamDefinition {
                parameter_id: u32::MAX,
                parameter_rate: 48000,
                mode: true,
                duration: 0,
                constant_subblock_duration: 0,
                subblock_durations: Vec::new(),
            },
            kind: PositionKind::Polar,
            defaults: vec![[0, 0, 127]; usize::from(num_objects)],
        });
    if param.num_objects() != usize::from(num_objects) {
        return Err(DecodeError::InvalidDescriptors(format!(
            "element {} has {num_objects} object(s) but its position parameter {}",
            sub_element.audio_element_id,
            param.num_objects()
        )));
    }
    Ok(crate::position::PositionCursor::new(&param))
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
    /// IAMF v2.0 element gain offset, linear (1.0 when absent).
    gain_offset: f32,
    /// Position timeline of an object-based element.
    position: Option<crate::position::PositionCursor>,
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
    /// Objects of the last pulled temporal unit (object passthrough).
    objects: Vec<DecodedObject>,
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
            if !declared.intersects(settings.selectable_profiles()) {
                return Err(DecodeError::UnsupportedProfile(format!(
                    "stream declares profiles {}/{} outside the requested set",
                    header.primary_profile, header.additional_profile
                )));
            }
        }
        // iamf-tools semantics: a mix presentation is selectable when it
        // fits within some requested profile's limits.
        let supported: Vec<bool> = parsed
            .mix_presentations
            .iter()
            .map(|mix| {
                !filter_profiles_for_mix(
                    mix,
                    &parsed.audio_elements,
                    &parsed.codec_configs,
                    settings.selectable_profiles(),
                )
                .is_empty()
            })
            .collect();
        let mix_index = select_mix_index(
            &parsed.mix_presentations,
            &supported,
            settings.mix_selection,
            settings.layout,
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
                gain_offset: sub_element.element_gain_offset.map_or(1.0, |offset| {
                    crate::params::q78_db_to_linear(offset.default_q78())
                }),
                position: match &element.config {
                    AudioElementConfig::ObjectBased { num_objects } => {
                        Some(object_position_cursor(sub_element, *num_objects)?)
                    }
                    _ => None,
                },
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
            objects: Vec::new(),
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
                ParamKind::Position => {
                    let Some(cursor) = slots[*slot_index].position.as_mut() else {
                        continue;
                    };
                    let context = ParamContext::Position {
                        kind: cursor.kind(),
                        objects: cursor.num_objects(),
                    };
                    let block =
                        ParameterBlock::parse(payload, definition, &context).map_err(corrupt)?;
                    for sb in block.subblocks {
                        if let SubblockData::Position(data) = sb.data {
                            cursor.push(data, sb.duration, scale);
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
        #[allow(clippy::type_complexity)]
        let mut pending_objects: Vec<(u32, u8, Vec<f32>, Vec<(u32, ObjectPosition)>)> = Vec::new();

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

            if let Some(cursor) = slot.position.as_mut() {
                // Object passthrough (the only way an object mix is
                // selected): gains now, output gain and trimming once the
                // whole unit is known, positions at the kept samples.
                let (start, end) = self.settings.trimming.window(trim, frame_len);
                let interval = self.settings.object_position_interval.max(1) as usize;
                let points: Vec<usize> = (start..frame_len - end).step_by(interval).collect();
                let positions = cursor.positions_for_unit(frame_len, &points);
                let gains: Vec<f32> = (0..frame_len)
                    .map(|_| slot.gain_cursor.next(slot.gain_default) * slot.gain_offset)
                    .collect();
                for (index, (plane, track)) in planes.into_iter().zip(positions).enumerate() {
                    let samples = plane.iter().zip(&gains).map(|(&s, &g)| s * g).collect();
                    pending_objects.push((
                        slot.element.audio_element_id,
                        index as u8,
                        samples,
                        points
                            .iter()
                            .map(|&p| (p - start) as u32)
                            .zip(track)
                            .collect(),
                    ));
                }
                continue;
            }

            let hrtf = cfg!(feature = "binaural")
                && self.target == SoundSystem::Binaural
                && slot.headphones_rendering_mode == 1;
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
                    let rendered = if hrtf {
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
                    let rendered = {
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
                    *o += g * slot.gain_offset * s;
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
        let (start, end) = self.settings.trimming.window(Some(trim), unit_len);
        let kept = unit_len - start - end;
        self.objects = pending_objects
            .into_iter()
            .map(
                |(audio_element_id, index, samples, positions)| DecodedObject {
                    audio_element_id,
                    index,
                    samples: samples
                        .iter()
                        .zip(&out_gains)
                        .take(unit_len - end)
                        .skip(start)
                        .map(|(&s, &g)| s * g * self.norm_gain)
                        .collect(),
                    positions,
                },
            )
            .collect();
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
            if let Some(cursor) = &mut slot.position {
                cursor.clear();
            }
            for dec in &mut slot.decoders {
                dec.reset();
            }
        }
        self.objects.clear();
    }

    /// The objects of the last temporal unit [`Self::get_output_temporal_unit`]
    /// returned, in mix order (object passthrough; empty otherwise).
    pub fn take_objects(&mut self) -> Vec<DecodedObject> {
        std::mem::take(&mut self.objects)
    }

    /// Objects the selected mix hands out per temporal unit (object
    /// passthrough), 0 when it has none.
    pub fn num_objects(&self) -> usize {
        self.slots
            .iter()
            .filter_map(|s| s.position.as_ref())
            .map(crate::position::PositionCursor::num_objects)
            .sum()
    }

    /// Whether the selected mix renders anything into the output layout,
    /// i.e. has elements other than objects.
    pub fn has_rendered_elements(&self) -> bool {
        self.slots.iter().any(|s| s.position.is_none())
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
    use iamf_obu::descriptors::{Layout, MixPresentation, SubMix};

    fn mix(id: u32, sound_system: u8) -> MixPresentation {
        use iamf_obu::descriptors::{MixGainParam, ParamDefinition};
        let gain = MixGainParam {
            base: ParamDefinition {
                parameter_id: 0,
                parameter_rate: 48000,
                mode: true,
                duration: 0,
                constant_subblock_duration: 0,
                subblock_durations: vec![],
            },
            default_mix_gain: 0,
        };
        MixPresentation {
            mix_presentation_id: id,
            annotation_languages: vec![],
            localized_annotations: vec![],
            sub_mixes: vec![SubMix {
                elements: vec![],
                output_mix_gain: gain,
                layouts: vec![(
                    Layout::LoudspeakersSsConvention { sound_system },
                    iamf_obu::descriptors::LoudnessInfo {
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
    fn mix_selection_modes() {
        let mixes = [mix(10, 0), mix(20, 9)];
        let all = [true, true];
        // Auto prefers the mix declaring the requested layout.
        assert_eq!(
            select_mix_index(&mixes, &all, MixSelection::Auto, SoundSystem::J).unwrap(),
            1
        );
        // Auto falls back to the first when nothing matches.
        assert_eq!(
            select_mix_index(&mixes, &all, MixSelection::Auto, SoundSystem::H).unwrap(),
            0
        );
        // Binaural playback matches stereo-authored mixes.
        assert_eq!(
            select_mix_index(&mixes, &all, MixSelection::Auto, SoundSystem::Binaural).unwrap(),
            0
        );
        assert_eq!(
            select_mix_index(&mixes, &all, MixSelection::ById(20), SoundSystem::A).unwrap(),
            1
        );
        // Unknown id falls back to automatic selection (iamf-tools
        // RequestedMix semantics).
        assert_eq!(
            select_mix_index(&mixes, &all, MixSelection::ById(99), SoundSystem::A).unwrap(),
            0
        );
        assert_eq!(
            select_mix_index(&mixes, &all, MixSelection::ByIndex(1), SoundSystem::A).unwrap(),
            1
        );
        assert!(select_mix_index(&mixes, &all, MixSelection::ByIndex(2), SoundSystem::A).is_err());
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
            select_mix_index(&mixes, &supported, MixSelection::Auto, SoundSystem::J).unwrap(),
            1
        );
        // An id resolving to an unsupported mix falls back to auto.
        assert_eq!(
            select_mix_index(&mixes, &supported, MixSelection::ById(10), SoundSystem::A).unwrap(),
            1
        );
        // Explicit index to an unsupported mix is an error.
        assert!(matches!(
            select_mix_index(&mixes, &supported, MixSelection::ByIndex(0), SoundSystem::A),
            Err(DecodeError::UnsupportedProfile(_))
        ));
        // Nothing supported at all.
        assert!(matches!(
            select_mix_index(&mixes, &[false, false], MixSelection::Auto, SoundSystem::A),
            Err(DecodeError::UnsupportedProfile(_))
        ));
    }
}
