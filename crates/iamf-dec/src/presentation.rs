//! Mix presentation orchestration: descriptors → per-element decode →
//! reconstruct → render → mix gains → summed output.
//!
//! Gain handling currently applies the static default mix gains from the
//! mix presentation. Animated gains from parameter blocks are parsed
//! (`crate::params`) but not yet applied here.

use iamf_obu::descriptors::{self, AudioElement, CodecConfig, Descriptor, MixPresentation};
use iamf_obu::{AudioFrame, ObuIter};

use crate::element::ElementDecoder;
use crate::layout::SoundSystem;
use crate::reconstruct::reconstruct;
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

struct ElementSlot {
    element: AudioElement,
    decoder: ElementDecoder,
    /// Linear default element mix gain.
    gain: f32,
}

/// Decodes and renders the first sub mix of one mix presentation to a
/// target sound system.
pub struct PresentationDecoder {
    slots: Vec<ElementSlot>,
    output_gain: f32,
    target: SoundSystem,
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
            slots.push(ElementSlot {
                element: element.clone(),
                decoder,
                gain: q78_db_to_linear(sub_element.element_mix_gain.default_mix_gain),
            });
        }
        Ok(PresentationDecoder {
            slots,
            output_gain: q78_db_to_linear(sub_mix.output_mix_gain.default_mix_gain),
            target,
        })
    }

    /// Routes one audio frame to the element that owns its substream.
    /// Returns whether any element consumed it.
    pub fn decode_frame(&mut self, frame: &AudioFrame<'_>) -> Result<bool, DecodeError> {
        for slot in &mut self.slots {
            if slot.decoder.decode_frame(frame)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Reconstructs, renders, applies gains, and sums all elements.
    pub fn finish(self) -> Result<RenderedMix, DecodeError> {
        let target_matrix = self.target.matrix_layout();
        let channels = self.target.channels();
        let mut mixed: Vec<Vec<f32>> = Vec::new();
        let mut sample_rate = 0;

        for slot in self.slots {
            let substreams = slot.decoder.finish();
            sample_rate = substreams
                .first()
                .map(|s| s.sample_rate)
                .unwrap_or(sample_rate);
            let reconstructed = reconstruct(&slot.element, &substreams)?;
            let rendered = render(&reconstructed, target_matrix)?;
            debug_assert_eq!(rendered.len(), channels);
            if mixed.is_empty() {
                mixed = vec![Vec::new(); rendered.len()];
            }
            for (mix_plane, rendered_plane) in mixed.iter_mut().zip(&rendered) {
                if mix_plane.len() < rendered_plane.len() {
                    mix_plane.resize(rendered_plane.len(), 0.0);
                }
                for (o, &s) in mix_plane.iter_mut().zip(rendered_plane.iter()) {
                    *o += slot.gain * s;
                }
            }
        }

        let frames = mixed.first().map(Vec::len).unwrap_or(0);
        let mut interleaved = vec![0.0f32; frames * mixed.len()];
        for (c, plane) in mixed.iter().enumerate() {
            for (t, &s) in plane.iter().enumerate() {
                interleaved[t * mixed.len() + c] = s * self.output_gain;
            }
        }
        Ok(RenderedMix {
            channels: mixed.len(),
            sample_rate,
            interleaved,
        })
    }
}
