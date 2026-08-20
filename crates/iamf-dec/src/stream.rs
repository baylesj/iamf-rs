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

#[derive(Debug, Clone, Copy)]
pub struct StreamSettings {
    pub layout: SoundSystem,
    pub sample_type: OutputSampleType,
    /// Index of the mix presentation to decode.
    pub mix_presentation_index: usize,
}

impl Default for StreamSettings {
    fn default() -> Self {
        StreamSettings {
            layout: SoundSystem::A,
            sample_type: OutputSampleType::Int16LittleEndian,
            mix_presentation_index: 0,
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

/// Streaming IAMF decoder for one mix presentation and output layout.
pub struct StreamDecoder {
    slots: Vec<SlotState>,
    param_index: HashMap<u32, (usize, ParamKind, ParamDefinition)>,
    target: SoundSystem,
    sample_type: OutputSampleType,
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
        let mix = parsed
            .mix_presentations
            .get(settings.mix_presentation_index)
            .ok_or(DecodeError::InvalidDescriptors(
                "no such mix presentation".into(),
            ))?;
        let [sub_mix] = mix.sub_mixes.as_slice() else {
            return Err(DecodeError::Unimplemented("multiple sub mixes"));
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
            if channels.len() != element.substream_ids.len() {
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

        Ok(StreamDecoder {
            slots,
            param_index,
            target: settings.layout,
            sample_type: settings.sample_type,
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

            let rendered = match &slot.element.config {
                AudioElementConfig::ChannelBased { layers } => {
                    if slot.reconstructor.is_none() {
                        let mut rec = ChannelReconstructor::new(layers, self.target, frame_len)?;
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
                    let reconstructed = crate::reconstruct::Reconstructed::Channels {
                        matrix: rec.matrix,
                        planar: rec.process_frame(&planes)?,
                    };
                    render(&reconstructed, target_matrix)?
                }
                _ => {
                    let reconstructed = ambisonics_from_planes(&slot.element.config, planes)?;
                    render(&reconstructed, target_matrix)?
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
        let start = (trim.0 as usize).min(unit_len);
        let end = (trim.1 as usize).min(unit_len - start);
        let kept = unit_len - start - end;
        let mut bytes =
            Vec::with_capacity(kept * out_channels * self.sample_type.bytes_per_sample());
        for (t, &gain) in out_gains
            .iter()
            .enumerate()
            .take(unit_len - end)
            .skip(start)
        {
            for plane in &mixed {
                let sample = plane.get(t).copied().unwrap_or(0.0) * gain;
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
            slot.gain_cursor = GainCursor::default();
            for dec in &mut slot.decoders {
                dec.reset();
            }
        }
    }
}
