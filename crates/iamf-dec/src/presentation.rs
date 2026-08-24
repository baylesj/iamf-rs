//! Mix presentation orchestration: descriptors → per-element decode →
//! reconstruct (frame-based demixing) → render → mix gains → summed output.
//!
//! Demixing-mode and recon-gain parameter blocks are applied per frame.
//! Element/output mix gains currently use the static defaults; animated
//! mix-gain parameter blocks are not yet applied.

use std::collections::HashMap;

use iamf_obu::descriptors::{
    self, AudioElement, CodecConfig, Descriptor, ElementParam, MixPresentation, ParamDefinition,
};
use iamf_obu::{AudioFrame, Obu, ObuIter, ObuType};

use crate::element::ElementDecoder;
use crate::layout::SoundSystem;
use crate::params::{ParamContext, ParameterBlock, ReconGainLayers, SubblockData};
use crate::reconstruct::{
    deinterleave, reconstruct_ambisonics, ChannelReconstructor, Reconstructed,
};
use crate::render::render;
use crate::{CodecFactory, DecodeError};

/// All descriptor OBUs of an IA sequence, first copy wins for redundant
/// re-transmissions.
#[derive(Default)]
pub struct Descriptors {
    pub codec_configs: Vec<CodecConfig>,
    pub audio_elements: Vec<AudioElement>,
    pub mix_presentations: Vec<MixPresentation>,
}

impl Descriptors {
    pub fn collect(data: &[u8]) -> Result<Self, DecodeError> {
        let mut out = Descriptors::default();
        for result in ObuIter::new(data) {
            let obu = result.map_err(|e| DecodeError::InvalidDescriptors(e.to_string()))?;
            match descriptors::parse(&obu)
                .map_err(|e| DecodeError::InvalidDescriptors(e.to_string()))?
            {
                Some(Descriptor::CodecConfig(cc))
                    if !out
                        .codec_configs
                        .iter()
                        .any(|c| c.codec_config_id == cc.codec_config_id) =>
                {
                    out.codec_configs.push(cc)
                }
                Some(Descriptor::AudioElement(ae))
                    if !out
                        .audio_elements
                        .iter()
                        .any(|e| e.audio_element_id == ae.audio_element_id) =>
                {
                    out.audio_elements.push(ae)
                }
                Some(Descriptor::MixPresentation(mp))
                    if !out
                        .mix_presentations
                        .iter()
                        .any(|m| m.mix_presentation_id == mp.mix_presentation_id) =>
                {
                    out.mix_presentations.push(mp)
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
}

/// Q7.8 dB → linear gain.
fn q78_db_to_linear(q: i16) -> f32 {
    10f32.powf(f32::from(q) / 256.0 / 20.0)
}

/// Final rendered output of one sub mix.
pub struct RenderedMix {
    pub channels: usize,
    pub sample_rate: u32,
    /// Interleaved f32 samples.
    pub interleaved: Vec<f32>,
}

/// Parameters snapshotted for one temporal unit of one element.
#[derive(Clone, Default)]
struct FrameParams {
    dmx_mode: Option<u8>,
    recon: Option<ReconGainLayers>,
}

struct ElementSlot {
    element: AudioElement,
    decoder: ElementDecoder,
    /// §3.8.2: 0 = stereo fallback for headphones, 1 = HRTF binaural.
    headphones_rendering_mode: u8,
    /// Linear default element mix gain.
    gain: f32,
    /// Parameter rate of the element mix gain parameter.
    gain_rate: u32,
    /// Animated mix gain blocks, in arrival order.
    gain_blocks: Vec<ParameterBlock>,
    current: FrameParams,
    /// One snapshot per temporal unit, taken when the first substream's
    /// frame arrives.
    frame_params: Vec<FrameParams>,
}

#[derive(Clone, Copy)]
enum ParamKind {
    Demixing,
    ReconGain,
    /// Mix gain of the slot's element.
    ElementMixGain,
    /// Output mix gain of the sub mix (slot index unused).
    OutputMixGain,
}

/// Decodes and renders the first sub mix of one mix presentation to a
/// target sound system.
pub struct PresentationDecoder {
    slots: Vec<ElementSlot>,
    output_gain: f32,
    output_gain_rate: u32,
    output_gain_blocks: Vec<ParameterBlock>,
    target: SoundSystem,
    /// parameter_id → (slot index, kind, definition).
    param_index: HashMap<u32, (usize, ParamKind, ParamDefinition)>,
}

impl PresentationDecoder {
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
        let [sub_mix] = mix.sub_mixes.as_slice() else {
            return Err(DecodeError::Unimplemented("multiple sub mixes"));
        };

        let mut slots = Vec::new();
        let mut param_index = HashMap::new();
        for sub_element in &sub_mix.elements {
            let element = descriptors
                .element(sub_element.audio_element_id)
                .ok_or_else(|| {
                    DecodeError::InvalidDescriptors(format!(
                        "mix references unknown element {}",
                        sub_element.audio_element_id
                    ))
                })?;
            let codec_config = descriptors
                .codec_config(element.codec_config_id)
                .ok_or_else(|| {
                    DecodeError::InvalidDescriptors(format!(
                        "element references unknown codec config {}",
                        element.codec_config_id
                    ))
                })?;
            let decoder = ElementDecoder::new(element, codec_config, factory)?;
            let slot_index = slots.len();
            for param in &element.params {
                match param {
                    ElementParam::Demixing { base, .. } => {
                        param_index.insert(
                            base.parameter_id,
                            (slot_index, ParamKind::Demixing, base.clone()),
                        );
                    }
                    ElementParam::ReconGain(base) => {
                        param_index.insert(
                            base.parameter_id,
                            (slot_index, ParamKind::ReconGain, base.clone()),
                        );
                    }
                    ElementParam::Unknown { .. } => {}
                }
            }
            param_index.insert(
                sub_element.element_mix_gain.base.parameter_id,
                (
                    slot_index,
                    ParamKind::ElementMixGain,
                    sub_element.element_mix_gain.base.clone(),
                ),
            );
            slots.push(ElementSlot {
                element: element.clone(),
                decoder,
                headphones_rendering_mode: sub_element.headphones_rendering_mode,
                gain: q78_db_to_linear(sub_element.element_mix_gain.default_mix_gain),
                gain_rate: sub_element.element_mix_gain.base.parameter_rate,
                gain_blocks: Vec::new(),
                current: FrameParams::default(),
                frame_params: Vec::new(),
            });
        }
        param_index.insert(
            sub_mix.output_mix_gain.base.parameter_id,
            (
                0,
                ParamKind::OutputMixGain,
                sub_mix.output_mix_gain.base.clone(),
            ),
        );
        Ok(PresentationDecoder {
            slots,
            output_gain: q78_db_to_linear(sub_mix.output_mix_gain.default_mix_gain),
            output_gain_rate: sub_mix.output_mix_gain.base.parameter_rate,
            output_gain_blocks: Vec::new(),
            target,
            param_index,
        })
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
        let Some((slot_index, kind, definition)) = self.param_index.get(&id).cloned() else {
            // Mix-gain and unknown parameters are not applied yet.
            return Ok(false);
        };
        let slot = &mut self.slots[slot_index];
        let context = match kind {
            ParamKind::Demixing => ParamContext::Demixing,
            ParamKind::ElementMixGain | ParamKind::OutputMixGain => ParamContext::MixGain,
            ParamKind::ReconGain => {
                let iamf_obu::descriptors::AudioElementConfig::ChannelBased { layers } =
                    &slot.element.config
                else {
                    return Ok(false);
                };
                ParamContext::ReconGain(layers)
            }
        };
        let block = ParameterBlock::parse(payload, &definition, &context)
            .map_err(|e| DecodeError::CorruptPacket(e.to_string()))?;
        match kind {
            ParamKind::ElementMixGain => {
                slot.gain_blocks.push(block);
                return Ok(true);
            }
            ParamKind::OutputMixGain => {
                self.output_gain_blocks.push(block);
                return Ok(true);
            }
            _ => {}
        }
        // Frame-aligned parameter blocks carry one subblock; when there are
        // several, the first is applied for the whole unit.
        match block.subblocks.first().map(|s| &s.data) {
            Some(SubblockData::Demixing { dmixp_mode }) => {
                slot.current.dmx_mode = Some(*dmixp_mode);
            }
            Some(SubblockData::ReconGain(layers)) => {
                slot.current.recon = Some(layers.clone());
            }
            _ => {}
        }
        Ok(true)
    }

    /// Routes one audio frame to the element that owns its substream.
    /// Returns whether any element consumed it.
    pub fn decode_frame(&mut self, frame: &AudioFrame<'_>) -> Result<bool, DecodeError> {
        for slot in &mut self.slots {
            if slot.decoder.is_first_substream(frame) {
                slot.frame_params.push(slot.current.clone());
            }
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
        let mut mixed: Vec<Vec<f32>> = Vec::new();
        let mut sample_rate = 0;
        let mut first_trim_map: Option<Vec<(usize, usize, usize)>> = None;

        for slot in self.slots {
            let gain = slot.gain;
            let gain_rate = slot.gain_rate;
            let gain_blocks = slot.gain_blocks.clone();
            let (slot_output, rate, trim_map) = reconstruct_slot(slot, target)?;
            if rate != 0 {
                sample_rate = rate;
            }
            if first_trim_map.is_none() {
                first_trim_map = Some(trim_map.clone());
            }
            let track = (!gain_blocks.is_empty())
                .then(|| evaluate_gain_track(&gain_blocks, gain, gain_rate, rate, &trim_map));
            let rendered = match slot_output {
                SlotOutput::Planar(reconstructed) => render(&reconstructed, target_matrix)?,
                SlotOutput::Stereo(stereo) => stereo,
            };
            if mixed.is_empty() {
                mixed = vec![Vec::new(); rendered.len()];
            }
            for (mix_plane, rendered_plane) in mixed.iter_mut().zip(&rendered) {
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

        let output_track = (!self.output_gain_blocks.is_empty())
            .then(|| {
                first_trim_map.as_ref().map(|map| {
                    evaluate_gain_track(
                        &self.output_gain_blocks,
                        self.output_gain,
                        self.output_gain_rate,
                        sample_rate,
                        map,
                    )
                })
            })
            .flatten();

        let frames = mixed.first().map(Vec::len).unwrap_or(0);
        let mut interleaved = vec![0.0f32; frames * mixed.len()];
        for (c, plane) in mixed.iter().enumerate() {
            for (t, &s) in plane.iter().enumerate() {
                let g = output_track
                    .as_ref()
                    .and_then(|tr| tr.get(t).copied())
                    .unwrap_or(self.output_gain);
                interleaved[t * mixed.len() + c] = s * g;
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
/// timeline described by `trim_map` (per temporal unit: untrimmed length,
/// start trim, end trim), returning gains aligned with the trimmed output.
fn evaluate_gain_track(
    blocks: &[ParameterBlock],
    default_gain: f32,
    parameter_rate: u32,
    sample_rate: u32,
    trim_map: &[(usize, usize, usize)],
) -> Vec<f32> {
    let total: usize = trim_map.iter().map(|&(len, _, _)| len).sum();
    // libiamf scales parameter durations by (rate + 0.1) / parameter_rate.
    let ratio = (f64::from(sample_rate) + 0.1) / f64::from(parameter_rate.max(1));
    let mut gains = vec![default_gain; total];
    let mut pos = 0usize;
    'blocks: for block in blocks {
        for sb in &block.subblocks {
            let duration = (sb.duration as f64 * ratio) as usize;
            if let SubblockData::MixGain(anim) = &sb.data {
                let end = (pos + duration).min(total);
                if pos < end {
                    anim.evaluate(duration, &mut gains[pos..end]);
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
    for &(len, start, end) in trim_map {
        track.extend_from_slice(&gains[off + start..off + len - end]);
        off += len;
    }
    track
}

/// Reconstructs one element: frame-based demixing for channel-based,
/// whole-buffer conversion for ambisonics. Returns the planar audio and
/// its sample rate.
type TrimMap = Vec<(usize, usize, usize)>;

/// Reconstructed element audio: planar (to be matrix-rendered) or already
/// binauralized stereo.
enum SlotOutput {
    Planar(Reconstructed),
    Stereo(Vec<Vec<f32>>),
}

/// Runs planes through the obr-style binaural renderer in fixed-size
/// blocks (untrimmed timeline; trims are applied afterwards).
#[cfg(feature = "binaural")]
fn binauralize(
    planes: &[Vec<f32>],
    input: crate::binaural::BinauralInput,
    frame_size: usize,
    sample_rate: u32,
) -> Result<Vec<Vec<f32>>, DecodeError> {
    let mut renderer = crate::binaural::BinauralRenderer::new(input, frame_size, sample_rate)?;
    let total = planes.first().map(Vec::len).unwrap_or(0);
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

/// Cuts trim spans out of planes, per temporal unit.
fn apply_trim_map(planes: Vec<Vec<f32>>, trim_map: &TrimMap) -> Vec<Vec<f32>> {
    planes
        .into_iter()
        .map(|plane| {
            let mut out = Vec::with_capacity(plane.len());
            let mut off = 0usize;
            for &(len, start, end) in trim_map {
                let upper = (off + len - end).min(plane.len());
                let lower = (off + start).min(upper);
                out.extend_from_slice(&plane[lower..upper]);
                off += len;
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
            let start = (f.trim_start as usize).min(count);
            let end = (f.trim_end as usize).min(count - start);
            (count, start, end)
        })
        .collect()
}

fn reconstruct_slot(
    slot: ElementSlot,
    target: SoundSystem,
) -> Result<(SlotOutput, u32, TrimMap), DecodeError> {
    use iamf_obu::descriptors::AudioElementConfig;

    let hrtf = cfg!(feature = "binaural")
        && target == SoundSystem::Binaural
        && slot.headphones_rendering_mode == 1;
    let ElementSlot {
        element,
        decoder,
        frame_params,
        ..
    } = slot;
    match &element.config {
        AudioElementConfig::ChannelBased { layers } => {
            let substreams = decoder.finish_frames();
            let sample_rate = substreams.first().map(|s| s.sample_rate).unwrap_or(0);
            let unit_count = substreams.iter().map(|s| s.frames.len()).min().unwrap_or(0);
            let frame_size = substreams
                .first()
                .and_then(|s| s.frames.first())
                .map(|f| f.samples.len() / usize::from(substreams[0].channels.max(1)))
                .unwrap_or(0);

            let mut rec =
                ChannelReconstructor::with_layer_selection(layers, target, frame_size, hrtf)?;
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
                if let Some(params) = frame_params.get(k) {
                    if let Some(mode) = params.dmx_mode {
                        rec.set_demixing_mode(mode)?;
                    }
                    if let Some(recon) = &params.recon {
                        rec.set_recon_gains(recon);
                    }
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
                let trim = &substreams[0].frames[k];
                let count = out.first().map(Vec::len).unwrap_or(0);
                let (start, end) = if hrtf {
                    (0, 0)
                } else {
                    let start = (trim.trim_start as usize).min(count);
                    (start, (trim.trim_end as usize).min(count - start))
                };
                if planar.is_empty() {
                    planar = vec![Vec::new(); out.len()];
                }
                for (acc, plane) in planar.iter_mut().zip(&out) {
                    acc.extend_from_slice(&plane[start..count - end]);
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
                    frame_size,
                    sample_rate,
                )?;
                let stereo = apply_trim_map(stereo, &trim_map);
                return Ok((SlotOutput::Stereo(stereo), sample_rate, trim_map));
            }
            Ok((
                SlotOutput::Planar(Reconstructed::Channels {
                    matrix: rec.matrix,
                    planar,
                }),
                sample_rate,
                trim_map,
            ))
        }
        AudioElementConfig::AmbisonicsMono { .. }
        | AudioElementConfig::AmbisonicsProjection { .. } => {
            let frames = decoder.finish_frames();
            let sample_rate = frames.first().map(|s| s.sample_rate).unwrap_or(0);
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
                let order = (planes.len() as f32).sqrt() as usize - 1;
                let frame_size = trim_map.first().map(|&(len, _, _)| len).unwrap_or(0);
                let stereo = binauralize(
                    planes,
                    crate::binaural::BinauralInput::Hoa { order },
                    frame_size,
                    sample_rate,
                )?;
                let stereo = apply_trim_map(stereo, &trim_map);
                return Ok((SlotOutput::Stereo(stereo), sample_rate, trim_map));
            }
            let substreams: Vec<_> = frames.iter().map(|f| f.trimmed()).collect();
            Ok((
                SlotOutput::Planar(reconstruct_ambisonics(&element, &substreams)?),
                sample_rate,
                trim_map,
            ))
        }
    }
}
