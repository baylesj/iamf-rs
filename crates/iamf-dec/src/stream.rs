//! Iterative (streaming) decoder, shaped after the iamf-tools decoder API
//! that Chromium's `IamfAudioDecoder` consumes: configure from a
//! descriptor blob, push arbitrary byte chunks (whole or partial OBUs),
//! and pull decoded temporal units as interleaved little-endian PCM.

use std::collections::{HashMap, VecDeque};

use iamf_obu::descriptors::{
    AudioElement, AudioElementConfig, CodecConfig, ElementParam, ParamDefinition,
};
use iamf_obu::{AudioFrame, ByteReader, Error, Obu, ObuType};

use crate::element::{substream_channels, FramePcm};
use crate::layout::SoundSystem;
use crate::params::{ParamContext, ParameterBlock, ReconGainLayers, SubblockData};
use crate::reconstruct::{ambisonics_from_planes, deinterleave, ChannelReconstructor};
use crate::render::render;
use crate::{CodecFactory, DecodeError, DecodedFrame, SubstreamDecoder};

/// Output PCM encoding (iamf-tools `OutputSampleType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputSampleType {
    Int16LittleEndian,
    Int32LittleEndian,
}

impl OutputSampleType {
    pub fn bytes_per_sample(self) -> usize {
        match self {
            OutputSampleType::Int16LittleEndian => 2,
            OutputSampleType::Int32LittleEndian => 4,
        }
    }
}

/// Output channel ordering (iamf-tools `ChannelOrdering`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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
#[derive(Debug, Clone, Copy)]
pub struct TrimmingSettings {
    pub trim_beginning: bool,
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

#[derive(Debug, Clone, Copy)]
pub struct StreamSettings {
    pub layout: SoundSystem,
    /// `None` selects s16le or s32le from the stream's bit depth.
    pub sample_type: Option<OutputSampleType>,
    /// Which mix presentation to decode.
    pub mix_selection: MixSelection,
    pub channel_ordering: ChannelOrdering,
    pub trimming: TrimmingSettings,
}

/// Mix presentation selection (iamf-tools `RequestedMix` shape).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MixSelection {
    /// Prefer a mix presentation that declares a layout matching the
    /// requested output layout; fall back to the first.
    #[default]
    Auto,
    /// Select by mix_presentation_id.
    ById(u32),
    /// Select by position in the descriptors.
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

/// Resolves a mix selection against parsed descriptors.
pub(crate) fn select_mix_index(
    mixes: &[iamf_obu::descriptors::MixPresentation],
    selection: MixSelection,
    target: SoundSystem,
) -> Result<usize, DecodeError> {
    match selection {
        MixSelection::ByIndex(index) => {
            if index < mixes.len() {
                Ok(index)
            } else {
                Err(DecodeError::InvalidDescriptors(
                    "no such mix presentation".into(),
                ))
            }
        }
        MixSelection::ById(id) => mixes
            .iter()
            .position(|m| m.mix_presentation_id == id)
            .ok_or(DecodeError::InvalidDescriptors(
                "no mix presentation with that id".into(),
            )),
        MixSelection::Auto => {
            // Binaural playback matches mixes authored for stereo.
            let wanted = match target {
                SoundSystem::Binaural => SoundSystem::A,
                other => other,
            };
            let declares_target = |m: &iamf_obu::descriptors::MixPresentation| {
                m.sub_mixes.iter().any(|sm| {
                    sm.layouts.iter().any(|(layout, _)| match layout {
                        iamf_obu::descriptors::Layout::LoudspeakersSsConvention {
                            sound_system,
                        } => SoundSystem::from_u8(*sound_system) == Some(wanted),
                        iamf_obu::descriptors::Layout::Binaural => target == SoundSystem::Binaural,
                        _ => false,
                    })
                })
            };
            Ok(mixes.iter().position(declares_target).unwrap_or(0))
        }
    }
}

/// Per-sample animated gain cursor: consumes subblocks in arrival order,
/// falling back to the default gain when exhausted.
#[derive(Default)]
struct GainCursor {
    queue: VecDeque<(crate::params::MixGainAnimation, usize, usize)>,
}

impl GainCursor {
    fn push(&mut self, anim: crate::params::MixGainAnimation, duration: usize) {
        if duration > 0 {
            self.queue.push_back((anim, duration, 0));
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

#[derive(Clone, Default)]
struct FrameParams {
    dmx_mode: Option<u8>,
    recon: Option<ReconGainLayers>,
}

struct SlotState {
    element: AudioElement,
    codec_config: CodecConfig,
    substream_ids: Vec<u32>,
    channels: Vec<u8>,
    decoders: Vec<Box<dyn SubstreamDecoder>>,
    /// One decoded-frame queue per substream.
    queues: Vec<VecDeque<FramePcm>>,
    params: VecDeque<FrameParams>,
    current: FrameParams,
    reconstructor: Option<ChannelReconstructor>,
    /// §3.8.2: 0 = stereo fallback for headphones, 1 = HRTF binaural.
    headphones_rendering_mode: u8,
    #[cfg(feature = "binaural")]
    binaural: Option<crate::binaural::BinauralRenderer>,
    gain_default: f32,
    gain_rate: u32,
    gain_cursor: GainCursor,
    sample_rate: u32,
}

impl SlotState {
    fn unit_ready(&self) -> bool {
        self.queues.iter().all(|q| !q.is_empty())
    }
}

#[derive(Clone, Copy)]
enum ParamKind {
    Demixing,
    ReconGain,
    ElementMixGain,
    OutputMixGain,
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

/// Streaming IAMF decoder for one mix presentation and output layout.
pub struct StreamDecoder {
    slots: Vec<SlotState>,
    param_index: HashMap<u32, (usize, ParamKind, ParamDefinition)>,
    target: SoundSystem,
    sample_type: OutputSampleType,
    /// Output channel permutation: slot i of the interleaved output takes
    /// rendered channel `permutation[i]`.
    permutation: Vec<usize>,
    trimming: TrimmingSettings,
    selected_mix_id: u32,
    output_gain_default: f32,
    output_gain_rate: u32,
    output_cursor: GainCursor,
    /// Buffered bytes of a partially received OBU.
    pending: Vec<u8>,
    frame_size: u32,
    ended: bool,
}

impl StreamDecoder {
    /// Creates a decoder from a descriptor blob (the descriptor OBUs of an
    /// IA sequence, e.g. from an ISO-BMFF `iacb` config box).
    pub fn new_from_descriptors(
        descriptors: &[u8],
        settings: StreamSettings,
        factory: &dyn CodecFactory,
    ) -> Result<Self, DecodeError> {
        let parsed = crate::presentation::Descriptors::collect(descriptors)?;
        if parsed.mix_presentations.is_empty() {
            return Err(DecodeError::InvalidDescriptors(
                "no mix presentations".into(),
            ));
        }
        let mix_index = select_mix_index(
            &parsed.mix_presentations,
            settings.mix_selection,
            settings.layout,
        )?;
        let mix = &parsed.mix_presentations[mix_index];
        let [sub_mix] = mix.sub_mixes.as_slice() else {
            // IAMF v1.1 requires num_sub_mixes == 1 in every profile
            // (libiamf rejects such streams outright as well).
            return Err(DecodeError::InvalidDescriptors(
                "IAMF v1.1 requires exactly one sub mix per mix presentation".into(),
            ));
        };

        let mut slots = Vec::new();
        let mut param_index = HashMap::new();
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
            let decoders = channels
                .iter()
                .map(|&ch| factory.create(codec_config, ch))
                .collect::<Result<Vec<_>, _>>()?;

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
                params: VecDeque::new(),
                current: FrameParams::default(),
                reconstructor: None,
                gain_default: crate::params::q78_db_to_linear(
                    sub_element.element_mix_gain.default_mix_gain,
                ),
                gain_rate: sub_element.element_mix_gain.base.parameter_rate,
                gain_cursor: GainCursor::default(),
                sample_rate: 0,
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
            param_index,
            target: settings.layout,
            sample_type,
            permutation: output_permutation(settings.layout, settings.channel_ordering),
            trimming: settings.trimming,
            selected_mix_id: mix.mix_presentation_id,
            output_gain_default: crate::params::q78_db_to_linear(
                sub_mix.output_mix_gain.default_mix_gain,
            ),
            output_gain_rate: sub_mix.output_mix_gain.base.parameter_rate,
            output_cursor: GainCursor::default(),
            pending: Vec::new(),
            frame_size,
            ended: false,
        })
    }

    /// Pushes bitstream bytes: whole or partial OBUs, as much or as little
    /// as the caller has. Decoded temporal units accumulate until pulled
    /// with [`StreamDecoder::get_output_temporal_unit`].
    pub fn decode(&mut self, data: &[u8]) -> Result<(), DecodeError> {
        self.pending.extend_from_slice(data);
        let mut consumed = 0usize;
        loop {
            let mut reader = ByteReader::new(&self.pending[consumed..]);
            match Obu::parse(&mut reader) {
                Ok(obu) => {
                    let advance = reader.position();
                    let obu_type = obu.header.obu_type;
                    let payload = obu.payload.to_vec();
                    let frame = AudioFrame::from_obu(&obu)
                        .map_err(|e| DecodeError::CorruptPacket(e.to_string()))?
                        .map(|f| {
                            (
                                f.substream_id,
                                f.data.to_vec(),
                                f.num_samples_to_trim_at_start,
                                f.num_samples_to_trim_at_end,
                            )
                        });
                    consumed += advance;
                    if let Some((id, data, trim_start, trim_end)) = frame {
                        self.handle_frame(id, &data, trim_start, trim_end)?;
                    } else if obu_type == ObuType::ParameterBlock {
                        self.handle_parameter_block(&payload)?;
                    }
                    // Descriptor OBUs after configuration are redundant
                    // copies; ignored.
                }
                Err(Error::UnexpectedEof { .. }) => break,
                Err(e) => return Err(DecodeError::CorruptPacket(e.to_string())),
            }
        }
        self.pending.drain(..consumed);
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
            if index == 0 {
                slot.params.push_back(slot.current.clone());
            }
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
        let Some((slot_index, kind, definition)) = self.param_index.get(&id).cloned() else {
            return Ok(());
        };
        let corrupt = |e: iamf_obu::Error| DecodeError::CorruptPacket(e.to_string());
        match kind {
            ParamKind::Demixing => {
                let block = ParameterBlock::parse(payload, &definition, &ParamContext::Demixing)
                    .map_err(corrupt)?;
                if let Some(SubblockData::Demixing { dmixp_mode }) =
                    block.subblocks.first().map(|s| &s.data)
                {
                    self.slots[slot_index].current.dmx_mode = Some(*dmixp_mode);
                }
            }
            ParamKind::ReconGain => {
                let AudioElementConfig::ChannelBased { layers } =
                    self.slots[slot_index].element.config.clone()
                else {
                    return Ok(());
                };
                let block =
                    ParameterBlock::parse(payload, &definition, &ParamContext::ReconGain(&layers))
                        .map_err(corrupt)?;
                if let Some(SubblockData::ReconGain(gains)) =
                    block.subblocks.first().map(|s| &s.data)
                {
                    self.slots[slot_index].current.recon = Some(gains.clone());
                }
            }
            ParamKind::ElementMixGain | ParamKind::OutputMixGain => {
                let block = ParameterBlock::parse(payload, &definition, &ParamContext::MixGain)
                    .map_err(corrupt)?;
                let rate = match kind {
                    ParamKind::ElementMixGain => self.slots[slot_index].gain_rate,
                    _ => self.output_gain_rate,
                };
                let ratio = (f64::from(self.sample_rate()) + 0.1) / f64::from(rate.max(1));
                let cursor = match kind {
                    ParamKind::ElementMixGain => &mut self.slots[slot_index].gain_cursor,
                    _ => &mut self.output_cursor,
                };
                for sb in &block.subblocks {
                    if let SubblockData::MixGain(anim) = &sb.data {
                        cursor.push(anim.clone(), (f64::from(sb.duration) * ratio) as usize);
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
        let mut trim = (0u32, 0u32);
        let mut unit_len = 0usize;

        for (i, slot) in self.slots.iter_mut().enumerate() {
            let frames: Vec<FramePcm> = slot
                .queues
                .iter_mut()
                .map(|q| q.pop_front().expect("unit_ready checked"))
                .collect();
            let params = slot.params.pop_front().unwrap_or_default();
            let frame_len = frames[0].samples.len() / usize::from(slot.channels[0].max(1));
            if i == 0 {
                trim = (frames[0].trim_start, frames[0].trim_end);
                unit_len = frame_len;
            }

            let mut planes = Vec::new();
            for (frame, &ch) in frames.iter().zip(&slot.channels) {
                planes.extend(deinterleave(&frame.samples, usize::from(ch.max(1))));
            }

            let hrtf = cfg!(feature = "binaural")
                && self.target == SoundSystem::Binaural
                && slot.headphones_rendering_mode == 1;
            let rendered = match &slot.element.config {
                AudioElementConfig::ChannelBased { layers } => {
                    if slot.reconstructor.is_none() {
                        let mut rec = ChannelReconstructor::with_layer_selection(
                            layers,
                            self.target,
                            frame_len,
                            hrtf,
                        )?;
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
                    if let Some(mode) = params.dmx_mode {
                        rec.set_demixing_mode(mode)?;
                    }
                    if let Some(recon) = &params.recon {
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
                            matrix: rec.matrix,
                            planar,
                        };
                        render(&reconstructed, target_matrix)?
                    };
                    #[cfg(not(feature = "binaural"))]
                    let rendered = {
                        let reconstructed = crate::reconstruct::Reconstructed::Channels {
                            matrix: rec.matrix,
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
                        let order = (hoa.len() as f32).sqrt() as usize - 1;
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

        // Output mix gain, then trimming, then interleave + quantize.
        let out_gains: Vec<f32> = (0..unit_len)
            .map(|_| self.output_cursor.next(self.output_gain_default))
            .collect();
        let start = if self.trimming.trim_beginning {
            (trim.0 as usize).min(unit_len)
        } else {
            0
        };
        let end = if self.trimming.trim_end {
            (trim.1 as usize).min(unit_len - start)
        } else {
            0
        };
        let kept = unit_len - start - end;
        let mut bytes =
            Vec::with_capacity(kept * out_channels * self.sample_type.bytes_per_sample());
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
                match self.sample_type {
                    OutputSampleType::Int16LittleEndian => {
                        let scaled = (sample * 32768.0).clamp(-32768.0, 32767.0);
                        bytes.extend((scaled.round_ties_even() as i16).to_le_bytes());
                    }
                    OutputSampleType::Int32LittleEndian => {
                        let scaled =
                            (f64::from(sample) * 2147483648.0).clamp(-2147483648.0, 2147483647.0);
                        bytes.extend((scaled.round_ties_even() as i32).to_le_bytes());
                    }
                }
            }
        }
        Ok(Some(bytes))
    }

    pub fn num_output_channels(&self) -> usize {
        self.target.channels()
    }

    pub fn sample_rate(&self) -> u32 {
        self.slots
            .iter()
            .map(|s| s.sample_rate)
            .find(|&r| r != 0)
            .unwrap_or_else(|| {
                self.slots
                    .first()
                    .map(|s| match &s.codec_config.decoder_config {
                        iamf_obu::descriptors::DecoderConfig::Opus { .. } => 48000,
                        iamf_obu::descriptors::DecoderConfig::Lpcm { sample_rate, .. } => {
                            *sample_rate
                        }
                        iamf_obu::descriptors::DecoderConfig::Flac { sample_rate, .. } => {
                            *sample_rate
                        }
                        _ => 0,
                    })
                    .unwrap_or(0)
            })
    }

    pub fn frame_size(&self) -> u32 {
        self.frame_size
    }

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
        for slot in &mut self.slots {
            for q in &mut slot.queues {
                q.clear();
            }
            slot.params.clear();
            slot.current = FrameParams::default();
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
        // Auto prefers the mix declaring the requested layout.
        assert_eq!(
            select_mix_index(&mixes, MixSelection::Auto, SoundSystem::J).unwrap(),
            1
        );
        // Auto falls back to the first when nothing matches.
        assert_eq!(
            select_mix_index(&mixes, MixSelection::Auto, SoundSystem::H).unwrap(),
            0
        );
        // Binaural playback matches stereo-authored mixes.
        assert_eq!(
            select_mix_index(&mixes, MixSelection::Auto, SoundSystem::Binaural).unwrap(),
            0
        );
        assert_eq!(
            select_mix_index(&mixes, MixSelection::ById(20), SoundSystem::A).unwrap(),
            1
        );
        assert!(select_mix_index(&mixes, MixSelection::ById(99), SoundSystem::A).is_err());
        assert_eq!(
            select_mix_index(&mixes, MixSelection::ByIndex(1), SoundSystem::A).unwrap(),
            1
        );
    }
}
