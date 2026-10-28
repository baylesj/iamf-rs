//! Mix presentation orchestration: descriptors → per-element decode →
//! reconstruct (frame-based demixing) → render → mix gains → summed output.
//!
//! Demixing-mode and recon-gain parameter blocks are applied per temporal
//! unit from a subblock timeline (a block whose subblocks span several
//! units applies each subblock to the units it covers). Element and output
//! mix gains are applied per sample, including step/linear/bezier
//! animations.

use iamf_obu::descriptors::{
    self, AudioElement, CodecConfig, Descriptor, ElementParam, MixPresentation,
};
use iamf_obu::{AudioFrame, Obu, ObuIter, ObuType};

use crate::element::ElementDecoder;
use crate::layout::{SoundSystem, element_renders_to};
use crate::params::{
    ParamContext, ParamIndex, ParamKind, ParameterBlock, SubblockData, build_param_index,
    q78_db_to_linear,
};
use crate::profile::{ProfileSet, filter_profiles_for_mix};
use crate::reconstruct::{
    ChannelReconstructor, Reconstructed, deinterleave, reconstruct_ambisonics,
};
use crate::render::render;
use crate::stream::{MixSelection, TrimmingSettings, select_mix_index};
use crate::{CodecFactory, DecodeError};

/// Error text for a sub mix without audio elements: §3.7 forbids
/// num_audio_elements == 0, and like iamf-tools (ac2fff70b) such a sub mix
/// is never rendered.
pub(crate) const EMPTY_SUB_MIX: &str =
    "sub mix has no audio elements (num_audio_elements SHALL NOT be 0)";

/// All descriptor OBUs of an IA sequence, first copy wins for redundant
/// re-transmissions — except that a non-redundant IA sequence header
/// supersedes a redundant one seen before any temporal unit.
#[derive(Debug, Clone, Default)]
pub struct Descriptors {
    /// Parsed sequence header descriptor, if present.
    pub sequence_header: Option<descriptors::SequenceHeader>,
    /// All unique codec config descriptors.
    pub codec_configs: Vec<CodecConfig>,
    /// All unique audio element descriptors.
    pub audio_elements: Vec<AudioElement>,
    /// All unique mix presentation descriptors.
    pub mix_presentations: Vec<MixPresentation>,
}

impl Descriptors {
    /// Collects and parses all descriptor OBUs from a raw byte stream.
    pub fn collect(data: &[u8]) -> Result<Self, DecodeError> {
        let mut out = Descriptors::default();
        // Whether the stored sequence header came from a redundant copy,
        // and whether any temporal-unit OBU has been seen yet.
        let mut sequence_header_is_redundant = false;
        let mut in_temporal_units = false;
        for result in ObuIter::new(data) {
            let obu = result.map_err(|e| DecodeError::InvalidDescriptors(e.to_string()))?;
            let obu_type = obu.header.obu_type;
            in_temporal_units |= obu_type.is_audio_frame()
                || matches!(
                    obu_type,
                    ObuType::ParameterBlock | ObuType::TemporalDelimiter
                );
            match descriptors::parse(&obu)
                .map_err(|e| DecodeError::InvalidDescriptors(e.to_string()))?
            {
                // §3.2: a redundant copy received without its non-redundant
                // original SHALL be processed as non-redundant, so it is
                // stored; but when the canonical (non-redundant) header then
                // arrives before any temporal unit, it is authoritative and
                // replaces the copy rather than starting a new IA sequence
                // (iamf-tools 23e4565dd; conformance vector test_000079).
                Some(Descriptor::SequenceHeader(sh))
                    if out.sequence_header.is_none()
                        || (sequence_header_is_redundant
                            && !obu.header.redundant_copy
                            && !in_temporal_units) =>
                {
                    out.sequence_header = Some(sh);
                    sequence_header_is_redundant = obu.header.redundant_copy;
                }
                Some(Descriptor::CodecConfig(cc))
                    if !out
                        .codec_configs
                        .iter()
                        .any(|c| c.codec_config_id == cc.codec_config_id) =>
                {
                    out.codec_configs.push(cc);
                }
                Some(Descriptor::AudioElement(ae))
                    if !out
                        .audio_elements
                        .iter()
                        .any(|e| e.audio_element_id == ae.audio_element_id) =>
                {
                    out.audio_elements.push(ae);
                }
                Some(Descriptor::MixPresentation(mp))
                    if !out
                        .mix_presentations
                        .iter()
                        .any(|m| m.mix_presentation_id == mp.mix_presentation_id) =>
                {
                    out.mix_presentations.push(mp);
                }
                _ => {}
            }
        }
        Ok(out)
    }

    fn element(&self, id: u32) -> Option<&AudioElement> {
        self.audio_elements
            .iter()
            .find(|e| e.audio_element_id == id)
    }

    fn codec_config(&self, id: u32) -> Option<&CodecConfig> {
        self.codec_configs.iter().find(|c| c.codec_config_id == id)
    }

    /// Resolves `selection` to a mix presentation index for rendering to
    /// `layout`, using the creator-preferred selection of IAMF §7.4.1 (the
    /// same rules [`StreamDecoder`](crate::stream::StreamDecoder) applies;
    /// see [`MixSelection`]). Only mixes within some profile of
    /// `requested_profiles`, and whose audio elements can be rendered to
    /// `layout`, are selectable: binaural-input elements only pass through
    /// to binaural or stereo output. Pass the result to
    /// [`PresentationDecoder::new`].
    pub fn select_mix_presentation(
        &self,
        selection: MixSelection,
        layout: SoundSystem,
        requested_profiles: ProfileSet,
    ) -> Result<usize, DecodeError> {
        let supported: Vec<bool> = self
            .mix_presentations
            .iter()
            .map(|mix| {
                !filter_profiles_for_mix(
                    mix,
                    &self.audio_elements,
                    &self.codec_configs,
                    requested_profiles,
                )
                .is_empty()
                    && mix
                        .sub_mixes
                        .iter()
                        .flat_map(|sub_mix| &sub_mix.elements)
                        .all(|sub_element| {
                            // Unknown elements are reported when decoding.
                            self.element(sub_element.audio_element_id)
                                .is_none_or(|e| element_renders_to(&e.config, layout))
                        })
            })
            .collect();
        select_mix_index(
            &self.mix_presentations,
            &self.audio_elements,
            &supported,
            selection,
            layout,
        )
    }
}

/// Final rendered output of one sub mix.
#[derive(Debug)]
pub struct RenderedMix {
    /// Number of output audio channels.
    pub channels: usize,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Interleaved f32 samples.
    pub interleaved: Vec<f32>,
}

struct ElementSlot {
    sub_mix_index: usize,
    element: AudioElement,
    decoder: ElementDecoder,
    /// §3.8.2: 0 = stereo fallback for headphones, 1/2 = HRTF binaural.
    headphones_rendering_mode: u8,
    binaural_filter_profile: descriptors::BinauralFilterProfile,
    /// Default element mix gain in Q7.8 dB.
    gain_db: i16,
    /// Linear default element mix gain.
    gain: f32,
    /// Parameter rate of the element mix gain parameter.
    gain_rate: u32,
    /// Animated mix gain blocks, in `(unit_index, block)` arrival order.
    gain_blocks: Vec<(usize, ParameterBlock)>,
    /// Demixing parameter blocks with their parameter rate, in arrival
    /// order; consumed as a subblock timeline at finish.
    dmx_blocks: Vec<(ParameterBlock, u32)>,
    /// Recon-gain parameter blocks, same shape.
    recon_blocks: Vec<(ParameterBlock, u32)>,
    /// Position parameter definition for object-based elements.
    position_def: Option<iamf_obu::position::PositionParamDefinition>,
    /// Position parameter blocks for object-based elements, in `(unit_index, block)` arrival order.
    position_blocks: Vec<(usize, ParameterBlock)>,
}

#[allow(clippy::struct_field_names)]
struct SubMixOutput {
    output_gain_db: i16,
    output_gain: f32,
    output_gain_rate: u32,
    output_gain_blocks: Vec<(usize, ParameterBlock)>,
}

impl core::fmt::Debug for PresentationDecoder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PresentationDecoder")
            .field("elements", &self.slots.len())
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

/// Decodes and renders the sub-mixes of one mix presentation to a
/// target sound system.
pub struct PresentationDecoder {
    slots: Vec<ElementSlot>,
    sub_mixes: Vec<SubMixOutput>,
    target: SoundSystem,
    /// See [`ParamIndex`].
    param_index: ParamIndex,
    /// Which audio-frame trims to honor (both by default).
    trimming: TrimmingSettings,
}

impl PresentationDecoder {
    /// Creates a new presentation decoder for the specified mix presentation index, target sound system, and codec factory.
    pub fn new(
        descriptors: &Descriptors,
        mix_presentation_index: usize,
        target: SoundSystem,
        factory: &dyn CodecFactory,
    ) -> Result<Self, DecodeError> {
        let mix = descriptors
            .mix_presentations
            .get(mix_presentation_index)
            .ok_or(DecodeError::InvalidDescriptors(
                "no such mix presentation".into(),
            ))?;
        if mix.sub_mixes.is_empty() || mix.sub_mixes.len() > 2 {
            return Err(DecodeError::InvalidDescriptors(
                "IAMF requires 1 or 2 sub mixes per mix presentation".into(),
            ));
        }
        if mix.sub_mixes.iter().any(|sm| sm.elements.is_empty()) {
            // Would otherwise "render" zero channels at rate 0.
            return Err(DecodeError::InvalidDescriptors(EMPTY_SUB_MIX.into()));
        }
        for sub_mix in &mix.sub_mixes {
            for sub_element in &sub_mix.elements {
                let element = descriptors
                    .element(sub_element.audio_element_id)
                    .ok_or_else(|| {
                        DecodeError::InvalidDescriptors(format!(
                            "mix references unknown element {}",
                            sub_element.audio_element_id
                        ))
                    })?;
                let _codec_config = descriptors
                    .codec_config(element.codec_config_id)
                    .ok_or_else(|| {
                        DecodeError::InvalidDescriptors(format!(
                            "element references unknown codec config {}",
                            element.codec_config_id
                        ))
                    })?;
                if matches!(
                    element.config,
                    descriptors::AudioElementConfig::ObjectBased { .. }
                ) && sub_element.position_params.is_empty()
                {
                    return Err(DecodeError::InvalidDescriptors(
                        "object-based element requires a position parameter definition".into(),
                    ));
                }
            }
        }

        let selected_sm = mix
            .sub_mixes
            .iter()
            .find(|sm| {
                sm.layouts.iter().any(|(layout, _)| match layout {
                    descriptors::Layout::LoudspeakersSsConvention { sound_system } => {
                        SoundSystem::from_bitstream_sound_system(*sound_system) == Some(target)
                    }
                    descriptors::Layout::Binaural => target == SoundSystem::Binaural,
                    descriptors::Layout::Reserved { .. } => false,
                })
            })
            .unwrap_or(&mix.sub_mixes[0]);
        let active_sub_mixes = std::slice::from_ref(selected_sm);

        let mut slots = Vec::new();
        let mut sub_mixes = Vec::with_capacity(active_sub_mixes.len());
        for (sm_idx, sub_mix) in active_sub_mixes.iter().enumerate() {
            for sub_element in &sub_mix.elements {
                let element = descriptors
                    .element(sub_element.audio_element_id)
                    .expect("validated above");
                let codec_config = descriptors
                    .codec_config(element.codec_config_id)
                    .expect("validated above");
                let decoder = ElementDecoder::new(element, codec_config, factory)?;
                slots.push(ElementSlot {
                    sub_mix_index: sm_idx,
                    element: element.clone(),
                    decoder,
                    headphones_rendering_mode: sub_element.headphones_rendering_mode,
                    binaural_filter_profile: sub_element.binaural_filter_profile,
                    gain_db: sub_element.element_mix_gain.default_mix_gain,
                    gain: q78_db_to_linear(sub_element.element_mix_gain.default_mix_gain),
                    gain_rate: sub_element.element_mix_gain.base.parameter_rate,
                    gain_blocks: Vec::new(),
                    dmx_blocks: Vec::new(),
                    recon_blocks: Vec::new(),
                    position_def: sub_element.position_params.first().cloned(),
                    position_blocks: Vec::new(),
                });
            }
            sub_mixes.push(SubMixOutput {
                output_gain_db: sub_mix.output_mix_gain.default_mix_gain,
                output_gain: q78_db_to_linear(sub_mix.output_mix_gain.default_mix_gain),
                output_gain_rate: sub_mix.output_mix_gain.base.parameter_rate,
                output_gain_blocks: Vec::new(),
            });
        }
        let param_index = build_param_index(active_sub_mixes, &descriptors.audio_elements)?;
        Ok(PresentationDecoder {
            slots,
            sub_mixes,
            target,
            param_index,
            trimming: TrimmingSettings::default(),
        })
    }

    /// Selects which audio-frame trims (`num_samples_to_trim_at_start` /
    /// `num_samples_to_trim_at_end`) are applied, mirroring
    /// [`StreamSettings::trimming`](crate::stream::StreamSettings::trimming)
    /// and iamf-tools `TrimmingSettings`. Both are applied by default;
    /// disable them when an outer layer (e.g. an MP4 demuxer honoring
    /// edts/elst) trims instead. Call before feeding any audio frames.
    #[must_use]
    pub fn with_trimming(mut self, trimming: TrimmingSettings) -> Self {
        self.trimming = trimming;
        self
    }

    /// Feeds one OBU: audio frames are decoded, demixing/recon-gain
    /// parameter blocks update per-frame state, other OBUs are ignored.
    /// Returns whether the OBU was consumed.
    pub fn process_obu(&mut self, obu: &Obu<'_>) -> Result<bool, DecodeError> {
        if obu.header.obu_type == ObuType::ParameterBlock {
            return self.process_parameter_block(obu.payload);
        }
        let Some(frame) =
            AudioFrame::from_obu(obu).map_err(|e| DecodeError::CorruptPacket(e.to_string()))?
        else {
            return Ok(false);
        };
        self.decode_frame(&frame)
    }

    fn process_parameter_block(&mut self, payload: &[u8]) -> Result<bool, DecodeError> {
        let id = ParameterBlock::peek_parameter_id(payload)
            .map_err(|e| DecodeError::CorruptPacket(e.to_string()))?;
        // Split borrows: the index is only read while slots are updated,
        // so no target list needs cloning.
        let PresentationDecoder {
            param_index,
            slots,
            sub_mixes,
            ..
        } = self;
        let Some(targets) = param_index.get(&id) else {
            return Ok(false);
        };
        let mut consumed = false;
        for (slot_index, kind, definition) in targets {
            match kind {
                ParamKind::OutputMixGain => {
                    let block = ParameterBlock::parse(payload, definition, &ParamContext::MixGain)
                        .map_err(|e| DecodeError::CorruptPacket(e.to_string()))?;
                    let unit_index = slots.first().map_or(0, |s| s.decoder.decoded_units());
                    sub_mixes[*slot_index]
                        .output_gain_blocks
                        .push((unit_index, block));
                }
                ParamKind::ElementMixGain => {
                    let block = ParameterBlock::parse(payload, definition, &ParamContext::MixGain)
                        .map_err(|e| DecodeError::CorruptPacket(e.to_string()))?;
                    let unit_index = slots[*slot_index].decoder.decoded_units();
                    slots[*slot_index].gain_blocks.push((unit_index, block));
                }
                ParamKind::Demixing => {
                    let block = ParameterBlock::parse(payload, definition, &ParamContext::Demixing)
                        .map_err(|e| DecodeError::CorruptPacket(e.to_string()))?;
                    slots[*slot_index]
                        .dmx_blocks
                        .push((block, definition.parameter_rate));
                }
                ParamKind::ReconGain => {
                    let slot = &mut slots[*slot_index];
                    let descriptors::AudioElementConfig::ChannelBased { layers } =
                        &slot.element.config
                    else {
                        continue;
                    };
                    let block = ParameterBlock::parse(
                        payload,
                        definition,
                        &ParamContext::ReconGain(layers),
                    )
                    .map_err(|e| DecodeError::CorruptPacket(e.to_string()))?;
                    slot.recon_blocks.push((block, definition.parameter_rate));
                }
                ParamKind::Position(param_type) => {
                    let block = ParameterBlock::parse(
                        payload,
                        definition,
                        &ParamContext::Position(*param_type),
                    )
                    .map_err(|e| DecodeError::CorruptPacket(e.to_string()))?;
                    let unit_index = slots[*slot_index].decoder.decoded_units();
                    slots[*slot_index].position_blocks.push((unit_index, block));
                }
            }
            consumed = true;
        }
        Ok(consumed)
    }

    /// Routes one audio frame to the element that owns its substream.
    /// Returns whether any element consumed it.
    pub fn decode_frame(&mut self, frame: &AudioFrame<'_>) -> Result<bool, DecodeError> {
        // Trims disabled via `with_trimming` are dropped before the element
        // decoder records them, so every downstream stage (reconstruction,
        // HRTF, gain timelines) sees the untrimmed timeline.
        let adjusted;
        let frame = if self.trimming.trim_beginning && self.trimming.trim_end {
            frame
        } else {
            let mut untrimmed = frame.clone();
            if !self.trimming.trim_beginning {
                untrimmed.num_samples_to_trim_at_start = 0;
            }
            if !self.trimming.trim_end {
                untrimmed.num_samples_to_trim_at_end = 0;
            }
            adjusted = untrimmed;
            &adjusted
        };
        for slot in &mut self.slots {
            if slot.decoder.decode_frame(frame)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Reconstructs, renders, applies gains, and sums all elements.
    pub fn finish(self) -> Result<RenderedMix, DecodeError> {
        let target = self.target;
        let target_matrix = target.matrix_layout();
        let num_sub_mixes = self.sub_mixes.len();
        let mut sub_mixed: Vec<Vec<Vec<f32>>> = vec![Vec::new(); num_sub_mixes];
        let mut sub_trim_maps: Vec<Option<TrimMap>> = vec![None; num_sub_mixes];
        let mut sample_rate = 0;
        let mut first_trim_map: Option<TrimMap> = None;

        for mut slot in self.slots {
            let sm_idx = slot.sub_mix_index;
            let gain_db = slot.gain_db;
            let gain = slot.gain;
            let gain_rate = slot.gain_rate;
            let gain_blocks = std::mem::take(&mut slot.gain_blocks);
            let (slot_output, rate, trim_map) = reconstruct_slot(slot, target)?;
            if rate != 0 {
                sample_rate = rate;
            }
            if first_trim_map.is_none() {
                first_trim_map = Some(trim_map.clone());
            }
            if sub_trim_maps[sm_idx].is_none() {
                sub_trim_maps[sm_idx] = Some(trim_map.clone());
            }
            let track = (!gain_blocks.is_empty())
                .then(|| evaluate_gain_track(&gain_blocks, gain_db, gain_rate, rate, &trim_map));
            let rendered = match slot_output {
                SlotOutput::Planar(reconstructed) => render(&reconstructed, target_matrix)?,
                SlotOutput::Rendered(rendered) => rendered,
            };
            let sm_planes = &mut sub_mixed[sm_idx];
            if sm_planes.is_empty() {
                *sm_planes = vec![Vec::new(); rendered.len()];
            }
            for (mix_plane, rendered_plane) in sm_planes.iter_mut().zip(&rendered) {
                if mix_plane.len() < rendered_plane.len() {
                    mix_plane.resize(rendered_plane.len(), 0.0);
                }
                match &track {
                    Some(track) => {
                        for (t, (o, &s)) in
                            mix_plane.iter_mut().zip(rendered_plane.iter()).enumerate()
                        {
                            *o += track.get(t).copied().unwrap_or(gain) * s;
                        }
                    }
                    None => {
                        for (o, &s) in mix_plane.iter_mut().zip(rendered_plane.iter()) {
                            *o += gain * s;
                        }
                    }
                }
            }
        }

        let mut mixed: Vec<Vec<f32>> = Vec::new();
        for (sm_idx, sm) in self.sub_mixes.into_iter().enumerate() {
            let sm_planes = &sub_mixed[sm_idx];
            if sm_planes.is_empty() {
                continue;
            }
            if mixed.is_empty() {
                mixed = vec![Vec::new(); sm_planes.len()];
            }
            let output_track = (!sm.output_gain_blocks.is_empty())
                .then(|| {
                    sub_trim_maps[sm_idx]
                        .as_ref()
                        .or(first_trim_map.as_ref())
                        .map(|map| {
                            evaluate_gain_track(
                                &sm.output_gain_blocks,
                                sm.output_gain_db,
                                sm.output_gain_rate,
                                sample_rate,
                                map,
                            )
                        })
                })
                .flatten();
            for (dst_plane, src_plane) in mixed.iter_mut().zip(sm_planes) {
                if dst_plane.len() < src_plane.len() {
                    dst_plane.resize(src_plane.len(), 0.0);
                }
                for (t, (d, &s)) in dst_plane.iter_mut().zip(src_plane.iter()).enumerate() {
                    let g = output_track
                        .as_ref()
                        .and_then(|tr| tr.get(t).copied())
                        .unwrap_or(sm.output_gain);
                    *d += s * g;
                }
            }
        }

        let frames = mixed.first().map_or(0, Vec::len);
        let mut interleaved = vec![0.0f32; frames * mixed.len()];
        for (c, plane) in mixed.iter().enumerate() {
            for (t, &s) in plane.iter().enumerate() {
                interleaved[t * mixed.len() + c] = s;
            }
        }
        Ok(RenderedMix {
            channels: mixed.len(),
            sample_rate,
            interleaved,
        })
    }
}

/// Evaluates an animated mix-gain timeline over the untrimmed sample
/// timeline described by `trim_map`, returning gains aligned with the
/// trimmed output.
fn evaluate_gain_track(
    blocks: &[(usize, ParameterBlock)],
    default_gain_db: i16,
    parameter_rate: u32,
    sample_rate: u32,
    trim_map: &[UnitTrim],
) -> Vec<f32> {
    let mut unit_offsets = Vec::with_capacity(trim_map.len() + 1);
    let mut total = 0usize;
    for t in trim_map {
        unit_offsets.push(total);
        total += t.len;
    }
    unit_offsets.push(total);
    let default_gain = q78_db_to_linear(default_gain_db);
    // libiamf scales parameter durations by (rate + 0.1) / parameter_rate.
    let ratio = (f64::from(sample_rate) + 0.1) / f64::from(parameter_rate.max(1));
    let mut gains = vec![default_gain; total];
    let mut pos = 0usize;
    let mut prev_end_db = default_gain_db;
    'blocks: for (unit_index, block) in blocks {
        if let Some(&unit_start) = unit_offsets.get(*unit_index)
            && unit_start > pos
        {
            pos = unit_start;
            prev_end_db = default_gain_db;
        }
        for sb in &block.subblocks {
            let duration = (sb.duration as f64 * ratio) as usize;
            if let SubblockData::MixGain(anim) = &sb.data {
                let (resolved, next_end_db) = anim.resolve(prev_end_db);
                prev_end_db = next_end_db;
                let end = (pos + duration).min(total);
                if pos < end {
                    resolved.evaluate(duration, &mut gains[pos..end]);
                }
            }
            pos += duration;
            if pos >= total {
                break 'blocks;
            }
        }
    }
    let mut track = Vec::with_capacity(total);
    let mut off = 0usize;
    for t in trim_map {
        track.extend_from_slice(&gains[off + t.start..off + t.len - t.end]);
        off += t.len;
    }
    track
}

/// Per-temporal-unit trim: the unit's untrimmed length and the leading /
/// trailing sample counts cut from it.
#[derive(Clone, Copy)]
struct UnitTrim {
    len: usize,
    start: usize,
    end: usize,
}

/// One [`UnitTrim`] per temporal unit of an element, in decode order.
type TrimMap = Vec<UnitTrim>;

/// Reconstructed element audio: planar (to be matrix-rendered) or final
/// rendered output (binauralized, panned objects, or passed-through binaural input).
enum SlotOutput {
    Planar(Reconstructed),
    Rendered(Vec<Vec<f32>>),
}

/// Runs planes through the obr-style binaural renderer in fixed-size
/// blocks (untrimmed timeline; trims are applied afterwards).
#[cfg(feature = "binaural")]
fn binauralize(
    planes: &[Vec<f32>],
    input: crate::binaural::BinauralInput,
    profile: crate::binaural::FilterProfile,
    frame_size: usize,
    sample_rate: u32,
) -> Result<Vec<Vec<f32>>, DecodeError> {
    let mut renderer =
        crate::binaural::BinauralRenderer::with_profile(input, profile, frame_size, sample_rate)?;
    let total = planes.first().map_or(0, Vec::len);
    let mut out = vec![Vec::with_capacity(total), Vec::with_capacity(total)];
    let mut pos = 0usize;
    while pos < total {
        let n = frame_size.min(total - pos);
        let chunk: Vec<Vec<f32>> = planes
            .iter()
            .map(|p| {
                let mut c = p[pos..pos + n].to_vec();
                c.resize(frame_size, 0.0);
                c
            })
            .collect();
        let stereo = renderer.process(&chunk)?;
        for (o, s) in out.iter_mut().zip(stereo.iter()) {
            o.extend_from_slice(&s[..n]);
        }
        pos += n;
    }
    Ok(out)
}

/// Runs object planes and per-unit positions through the obr-style binaural
/// renderer (untrimmed timeline; trims are applied afterwards).
#[cfg(feature = "binaural")]
fn binauralize_objects(
    planes: &[Vec<f32>],
    num_objects: u8,
    profile: crate::binaural::FilterProfile,
    unit_positions: &[Vec<crate::objects::PolarCoord>],
    unit_lengths: &[usize],
    sample_rate: u32,
) -> Result<Vec<Vec<f32>>, DecodeError> {
    let frame_size = unit_lengths.first().copied().unwrap_or(0);
    if frame_size == 0 {
        return Ok(vec![Vec::new(), Vec::new()]);
    }
    let mut renderer = crate::binaural::BinauralRenderer::with_profile(
        crate::binaural::BinauralInput::Objects {
            num_objects: usize::from(num_objects),
        },
        profile,
        frame_size,
        sample_rate,
    )?;
    let total = planes.first().map_or(0, Vec::len);
    let mut out = vec![Vec::with_capacity(total), Vec::with_capacity(total)];
    let mut pos = 0usize;
    for (&n, positions) in unit_lengths.iter().zip(unit_positions.iter()) {
        if pos >= total || n == 0 {
            break;
        }
        let n = n.min(total - pos);
        let chunk: Vec<Vec<f32>> = planes
            .iter()
            .map(|p| {
                let mut c = p[pos..pos + n].to_vec();
                c.resize(frame_size.max(n), 0.0);
                c
            })
            .collect();
        let stereo = renderer.process_objects(&chunk, positions)?;
        for (o, s) in out.iter_mut().zip(stereo.iter()) {
            o.extend_from_slice(&s[..n]);
        }
        pos += n;
    }
    Ok(out)
}

/// Cuts trim spans out of planes, per temporal unit.
#[cfg(feature = "binaural")]
fn apply_trim_map(planes: Vec<Vec<f32>>, trim_map: &TrimMap) -> Vec<Vec<f32>> {
    planes
        .into_iter()
        .map(|plane| {
            let mut out = Vec::with_capacity(plane.len());
            let mut off = 0usize;
            for t in trim_map {
                let upper = (off + t.len - t.end).min(plane.len());
                let lower = (off + t.start).min(upper);
                out.extend_from_slice(&plane[lower..upper]);
                off += t.len;
            }
            out
        })
        .collect()
}

fn trim_map_of(frames: &[crate::element::FramePcm], channels: usize) -> TrimMap {
    frames
        .iter()
        .map(|f| {
            let count = f.samples.len() / channels.max(1);
            let kept = f.kept_range(channels);
            UnitTrim {
                len: count,
                start: kept.start,
                end: count - kept.end,
            }
        })
        .collect()
}

/// Reconstructs one element: frame-based demixing for channel-based,
/// whole-buffer conversion for ambisonics, or object panning/binauralizing
/// for object-based. Returns the rendered-or-planar audio, its sample rate,
/// and the per-unit trim map.
fn reconstruct_slot(
    slot: ElementSlot,
    target: SoundSystem,
) -> Result<(SlotOutput, u32, TrimMap), DecodeError> {
    use iamf_obu::descriptors::AudioElementConfig;

    let is_object = matches!(slot.element.config, AudioElementConfig::ObjectBased { .. });
    // Binaural input is already headphone audio: never HRTF it.
    let hrtf = cfg!(feature = "binaural")
        && target == SoundSystem::Binaural
        && (matches!(slot.headphones_rendering_mode, 1 | 2) || is_object)
        && !crate::layout::is_binaural_input(&slot.element.config);
    #[cfg(feature = "binaural")]
    let filter_profile = crate::binaural::FilterProfile::from(slot.binaural_filter_profile);
    let ElementSlot {
        element,
        decoder,
        dmx_blocks,
        recon_blocks,
        position_def,
        position_blocks,
        ..
    } = slot;
    match &element.config {
        AudioElementConfig::ChannelBased { layers } => {
            let substreams = decoder.finish_frames();
            let sample_rate = substreams.first().map_or(0, |s| s.sample_rate);

            // Subblock timelines over the sample clock; a block spanning
            // several temporal units applies each subblock to the units it
            // covers (libiamf scales durations by (rate + 0.1) / param_rate).
            let scale = |parameter_rate: u32| {
                if sample_rate == 0 {
                    1.0
                } else {
                    (f64::from(sample_rate) + 0.1) / f64::from(parameter_rate.max(1))
                }
            };
            let mut dmx_cursor = crate::params::ParamCursor::default();
            for (block, rate) in &dmx_blocks {
                for sb in &block.subblocks {
                    if let SubblockData::Demixing { dmixp_mode } = &sb.data {
                        dmx_cursor.push(
                            *dmixp_mode,
                            (f64::from(sb.duration) * scale(*rate)) as usize,
                        );
                    }
                }
            }
            let mut recon_cursor = crate::params::ParamCursor::default();
            for (block, rate) in &recon_blocks {
                for sb in &block.subblocks {
                    if let SubblockData::ReconGain(gains) = &sb.data {
                        recon_cursor.push(
                            gains.clone(),
                            (f64::from(sb.duration) * scale(*rate)) as usize,
                        );
                    }
                }
            }
            let unit_count = substreams.iter().map(|s| s.frames.len()).min().unwrap_or(0);
            #[cfg(feature = "binaural")]
            let frame_size = substreams
                .first()
                .and_then(|s| s.frames.first())
                .map_or(0, |f| {
                    f.samples.len() / usize::from(substreams[0].channels.max(1))
                });

            let mut rec = ChannelReconstructor::with_layer_selection(layers, target, hrtf)?;
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

            let mut planar: Vec<Vec<f32>> = Vec::new();
            for k in 0..unit_count {
                let unit_len = substreams[0].frames[k].samples.len()
                    / usize::from(substreams[0].channels.max(1));
                if let Some(mode) = dmx_cursor.take_for_unit(unit_len) {
                    rec.set_demixing_mode(mode)?;
                }
                if let Some(recon) = recon_cursor.take_for_unit(unit_len) {
                    rec.set_recon_gains(&recon);
                }
                let mut planes = Vec::new();
                for sub in &substreams {
                    let frame = &sub.frames[k];
                    planes.extend(deinterleave(
                        &frame.samples,
                        usize::from(sub.channels.max(1)),
                    ));
                }
                let out = rec.process_frame(&planes)?;

                // For the HRTF path the untrimmed timeline is kept and
                // trimmed after convolution; otherwise trim per unit
                // (frame-level trims are identical across substreams).
                let count = out.first().map_or(0, Vec::len);
                let kept = if hrtf {
                    0..count
                } else {
                    let r = substreams[0].frames[k]
                        .kept_range(usize::from(substreams[0].channels.max(1)));
                    r.start.min(count)..r.end.min(count)
                };
                if planar.is_empty() {
                    planar = vec![Vec::new(); out.len()];
                }
                for (acc, plane) in planar.iter_mut().zip(&out) {
                    acc.extend_from_slice(&plane[kept.clone()]);
                }
            }
            let trim_map = substreams
                .first()
                .map(|s| trim_map_of(&s.frames, usize::from(s.channels.max(1))))
                .unwrap_or_default();
            #[cfg(feature = "binaural")]
            if hrtf {
                let stereo = binauralize(
                    &planar,
                    crate::binaural::BinauralInput::Speakers {
                        loudspeaker_layout: rec.layout(),
                    },
                    filter_profile,
                    frame_size,
                    sample_rate,
                )?;
                let stereo = apply_trim_map(stereo, &trim_map);
                return Ok((SlotOutput::Rendered(stereo), sample_rate, trim_map));
            }
            if rec.is_binaural_input() {
                // Passthrough (target is binaural or stereo).
                return Ok((SlotOutput::Rendered(planar), sample_rate, trim_map));
            }
            Ok((
                SlotOutput::Planar(Reconstructed::Channels {
                    matrix: rec.matrix(),
                    planar,
                }),
                sample_rate,
                trim_map,
            ))
        }
        AudioElementConfig::AmbisonicsMono { .. }
        | AudioElementConfig::AmbisonicsProjection { .. } => {
            let frames = decoder.finish_frames();
            let sample_rate = frames.first().map_or(0, |s| s.sample_rate);
            let trim_map = frames
                .first()
                .map(|s| trim_map_of(&s.frames, usize::from(s.channels.max(1))))
                .unwrap_or_default();
            #[cfg(feature = "binaural")]
            if hrtf {
                // Untrimmed planes through the binaural renderer, then trim.
                let mut untrimmed: Vec<crate::element::SubstreamPcm> = Vec::new();
                for f in &frames {
                    let mut samples = Vec::new();
                    for frame in &f.frames {
                        samples.extend_from_slice(&frame.samples);
                    }
                    untrimmed.push(crate::element::SubstreamPcm {
                        substream_id: f.substream_id,
                        channels: f.channels,
                        sample_rate: f.sample_rate,
                        samples,
                    });
                }
                let reconstructed = reconstruct_ambisonics(&element, &untrimmed)?;
                let planes = reconstructed.planar();
                let order = crate::reconstruct::hoa_order_index(planes.len());
                let frame_size = trim_map.first().map_or(0, |t| t.len);
                let stereo = binauralize(
                    planes,
                    crate::binaural::BinauralInput::Hoa { order },
                    filter_profile,
                    frame_size,
                    sample_rate,
                )?;
                let stereo = apply_trim_map(stereo, &trim_map);
                return Ok((SlotOutput::Rendered(stereo), sample_rate, trim_map));
            }
            let substreams: Vec<_> = frames
                .iter()
                .map(crate::element::SubstreamFrames::trimmed)
                .collect();
            Ok((
                SlotOutput::Planar(reconstruct_ambisonics(&element, &substreams)?),
                sample_rate,
                trim_map,
            ))
        }
        AudioElementConfig::ObjectBased { num_objects, .. } => {
            let substreams = decoder.finish_frames();
            let sample_rate = substreams.first().map_or(0, |s| s.sample_rate);
            let trim_map = substreams
                .first()
                .map(|s| trim_map_of(&s.frames, usize::from(s.channels.max(1))))
                .unwrap_or_default();
            let unit_lengths: Vec<usize> = trim_map.iter().map(|t| t.len).collect();
            let pos_def = position_def.as_ref().ok_or_else(|| {
                DecodeError::InvalidDescriptors(
                    "object-based element requires a position parameter definition".into(),
                )
            })?;
            let unit_positions = crate::objects::evaluate_position_track(
                *num_objects,
                pos_def,
                &position_blocks,
                sample_rate,
                &unit_lengths,
            )?;
            let unit_count = substreams.iter().map(|s| s.frames.len()).min().unwrap_or(0);

            #[cfg(feature = "binaural")]
            if hrtf {
                let mut untrimmed_planes: Vec<Vec<f32>> =
                    vec![Vec::new(); usize::from(*num_objects)];
                for k in 0..unit_count {
                    let mut planes = Vec::new();
                    for sub in &substreams {
                        let frame = &sub.frames[k];
                        planes.extend(deinterleave(
                            &frame.samples,
                            usize::from(sub.channels.max(1)),
                        ));
                    }
                    for (acc, plane) in untrimmed_planes.iter_mut().zip(&planes) {
                        acc.extend_from_slice(plane);
                    }
                }
                let stereo = binauralize_objects(
                    &untrimmed_planes,
                    *num_objects,
                    filter_profile,
                    &unit_positions,
                    &unit_lengths,
                    sample_rate,
                )?;
                let stereo = apply_trim_map(stereo, &trim_map);
                return Ok((SlotOutput::Rendered(stereo), sample_rate, trim_map));
            }

            let speaker_target = if target == SoundSystem::Binaural {
                SoundSystem::A
            } else {
                target
            };
            let mut obj_renderer = crate::objects::ObjectLoudspeakerRenderer::new(
                speaker_target,
                usize::from(*num_objects),
            )?;
            let mut planar: Vec<Vec<f32>> = vec![Vec::new(); speaker_target.channels()];
            for k in 0..unit_count {
                let mut planes = Vec::new();
                for sub in &substreams {
                    let frame = &sub.frames[k];
                    planes.extend(deinterleave(
                        &frame.samples,
                        usize::from(sub.channels.max(1)),
                    ));
                }
                let out = obj_renderer.process_frame(&planes, &unit_positions[k]);
                let count = out.first().map_or(0, Vec::len);
                let r =
                    substreams[0].frames[k].kept_range(usize::from(substreams[0].channels.max(1)));
                let kept = r.start.min(count)..r.end.min(count);
                for (acc, plane) in planar.iter_mut().zip(&out) {
                    acc.extend_from_slice(&plane[kept.clone()]);
                }
            }
            Ok((SlotOutput::Rendered(planar), sample_rate, trim_map))
        }
        _ => Err(DecodeError::Unimplemented(
            "rendering reserved audio elements",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// IA sequence header OBU declaring the given profiles.
    fn sequence_header_obu(redundant: bool, primary: u8, additional: u8) -> Vec<u8> {
        let mut out = vec![31 << 3 | u8::from(redundant) << 2, 6];
        out.extend(b"iamf");
        out.extend([primary, additional]);
        out
    }

    const TEMPORAL_DELIMITER: [u8; 2] = [4 << 3, 0];

    fn collected_profiles(data: &[u8]) -> (u8, u8) {
        let header = Descriptors::collect(data)
            .unwrap()
            .sequence_header
            .expect("sequence header collected");
        (header.primary_profile, header.additional_profile)
    }

    /// test_000079 shape, but with a copy that disagrees: the canonical
    /// (non-redundant) header wins over a redundant copy sent before it.
    #[test]
    fn canonical_sequence_header_supersedes_earlier_redundant_copy() {
        let mut data = sequence_header_obu(true, 2, 2);
        data.extend(sequence_header_obu(false, 0, 1));
        assert_eq!(collected_profiles(&data), (0, 1));
    }

    /// §3.2: a redundant copy without its original is processed as
    /// non-redundant.
    #[test]
    fn lone_redundant_sequence_header_is_used() {
        assert_eq!(collected_profiles(&sequence_header_obu(true, 2, 2)), (2, 2));
    }

    #[test]
    fn redundant_copy_does_not_replace_canonical_sequence_header() {
        let mut data = sequence_header_obu(false, 0, 1);
        data.extend(sequence_header_obu(true, 2, 2));
        assert_eq!(collected_profiles(&data), (0, 1));
    }

    /// Once temporal units have started, a non-redundant header belongs to
    /// a following IA sequence and does not rewrite this one.
    #[test]
    fn sequence_header_after_temporal_units_is_not_canonical() {
        let mut data = sequence_header_obu(true, 2, 2);
        data.extend(TEMPORAL_DELIMITER);
        data.extend(sequence_header_obu(false, 0, 1));
        assert_eq!(collected_profiles(&data), (2, 2));
    }

    /// A second non-redundant header (a new IA sequence per §3.4) keeps
    /// the first one, as before.
    #[test]
    fn second_non_redundant_sequence_header_is_ignored() {
        let mut data = sequence_header_obu(false, 0, 1);
        data.extend(sequence_header_obu(false, 2, 2));
        assert_eq!(collected_profiles(&data), (0, 1));
    }
}
