//! Substream-by-substream decoding of one audio element.

use iamf_obu::descriptors::{AudioElement, AudioElementConfig, CodecConfig};
use iamf_obu::AudioFrame;

use crate::{CodecFactory, DecodeError, DecodedFrame, SubstreamDecoder};

/// Channel count of each substream of an element, in `substream_ids` order.
/// Coupled substreams (stereo) come first within each layer / stream group
/// (IAMF §3.7): a group with `substream_count` streams of which
/// `coupled_substream_count` are coupled carries
/// `substream_count + coupled_substream_count` channels.
pub fn substream_channels(config: &AudioElementConfig) -> Vec<u8> {
    fn group(substreams: u8, coupled: u8) -> impl Iterator<Item = u8> {
        (0..substreams).map(move |i| if i < coupled { 2 } else { 1 })
    }
    match config {
        AudioElementConfig::ChannelBased { layers } => layers
            .iter()
            .flat_map(|l| group(l.substream_count, l.coupled_substream_count))
            .collect(),
        AudioElementConfig::AmbisonicsMono {
            substream_count, ..
        } => {
            vec![1; usize::from(*substream_count)]
        }
        AudioElementConfig::AmbisonicsProjection {
            substream_count,
            coupled_substream_count,
            ..
        } => group(*substream_count, *coupled_substream_count).collect(),
    }
}

/// One decoded, untrimmed frame of one substream (interleaved).
#[derive(Debug, Clone, Default)]
pub struct FramePcm {
    pub samples: Vec<f32>,
    pub trim_start: u32,
    pub trim_end: u32,
}

/// Decoded PCM of one substream.
#[derive(Debug, Clone, Default)]
pub struct SubstreamPcm {
    pub substream_id: u32,
    pub channels: u8,
    pub sample_rate: u32,
    /// Interleaved samples with per-frame trimming applied.
    pub samples: Vec<f32>,
}

/// Per-substream frames, untrimmed, for frame-based post-processing
/// (scalable demixing).
#[derive(Debug, Clone, Default)]
pub struct SubstreamFrames {
    pub substream_id: u32,
    pub channels: u8,
    pub sample_rate: u32,
    pub frames: Vec<FramePcm>,
}

impl SubstreamFrames {
    pub fn trimmed(&self) -> SubstreamPcm {
        let channels = usize::from(self.channels.max(1));
        let mut samples = Vec::new();
        for frame in &self.frames {
            let count = frame.samples.len() / channels;
            let start = (frame.trim_start as usize).min(count);
            let end = (frame.trim_end as usize).min(count - start);
            samples.extend_from_slice(&frame.samples[start * channels..(count - end) * channels]);
        }
        SubstreamPcm {
            substream_id: self.substream_id,
            channels: self.channels,
            sample_rate: self.sample_rate,
            samples,
        }
    }
}

/// Decodes every substream of a single audio element. Output is raw
/// substream PCM in `substream_ids` order; reconstruction/rendering happen
/// downstream.
pub struct ElementDecoder {
    substream_ids: Vec<u32>,
    decoders: Vec<Box<dyn SubstreamDecoder>>,
    outputs: Vec<SubstreamFrames>,
    scratch: DecodedFrame,
}

impl ElementDecoder {
    pub fn new(
        element: &AudioElement,
        codec_config: &CodecConfig,
        factory: &dyn CodecFactory,
    ) -> Result<Self, DecodeError> {
        if !factory.supports(codec_config) {
            return Err(DecodeError::UnsupportedCodec);
        }
        let channels = substream_channels(&element.config);
        if channels.len() != element.substream_ids.len() {
            return Err(DecodeError::InvalidDescriptors(format!(
                "element {} declares {} substream ids but its config implies {}",
                element.audio_element_id,
                element.substream_ids.len(),
                channels.len()
            )));
        }
        let decoders = channels
            .iter()
            .map(|&ch| factory.create(codec_config, ch))
            .collect::<Result<Vec<_>, _>>()?;
        let outputs = element
            .substream_ids
            .iter()
            .zip(&channels)
            .map(|(&id, &ch)| SubstreamFrames {
                substream_id: id,
                channels: ch,
                ..SubstreamFrames::default()
            })
            .collect();
        Ok(ElementDecoder {
            substream_ids: element.substream_ids.clone(),
            decoders,
            outputs,
            scratch: DecodedFrame::default(),
        })
    }

    /// Whether this frame belongs to the element's first substream (used to
    /// snapshot per-temporal-unit parameters).
    pub fn is_first_substream(&self, frame: &AudioFrame<'_>) -> bool {
        self.substream_ids.first() == Some(&frame.substream_id)
    }

    /// Decodes one audio frame if it belongs to this element. Returns
    /// whether the frame was consumed.
    pub fn decode_frame(&mut self, frame: &AudioFrame<'_>) -> Result<bool, DecodeError> {
        let Some(index) = self
            .substream_ids
            .iter()
            .position(|&id| id == frame.substream_id)
        else {
            return Ok(false);
        };
        self.decoders[index].decode(frame.data, &mut self.scratch)?;
        let out = &mut self.outputs[index];
        out.channels = self.scratch.channels;
        out.sample_rate = self.scratch.sample_rate;
        out.frames.push(FramePcm {
            samples: std::mem::take(&mut self.scratch.samples),
            trim_start: frame.num_samples_to_trim_at_start,
            trim_end: frame.num_samples_to_trim_at_end,
        });
        Ok(true)
    }

    /// Finishes decoding and yields trimmed per-substream PCM.
    pub fn finish(self) -> Vec<SubstreamPcm> {
        self.outputs.iter().map(SubstreamFrames::trimmed).collect()
    }

    /// Finishes decoding and yields per-substream frames (untrimmed).
    pub fn finish_frames(self) -> Vec<SubstreamFrames> {
        self.outputs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channels_for_channel_based_layers() {
        use iamf_obu::descriptors::ChannelAudioLayer;
        // Layer 1: stereo (1 coupled substream); layer 2: +4 substreams of
        // which 2 coupled (5.1 upgrade).
        let layer = |substreams, coupled| ChannelAudioLayer {
            loudspeaker_layout: 1,
            substream_count: substreams,
            coupled_substream_count: coupled,
            recon_gain_is_present: false,
            output_gain: None,
            expanded_loudspeaker_layout: None,
        };
        let config = AudioElementConfig::ChannelBased {
            layers: vec![layer(1, 1), layer(3, 2)],
        };
        assert_eq!(substream_channels(&config), vec![2, 2, 2, 1]);
    }

    #[test]
    fn channels_for_ambisonics() {
        let mono = AudioElementConfig::AmbisonicsMono {
            output_channel_count: 4,
            substream_count: 4,
            channel_mapping: vec![0, 1, 2, 3],
        };
        assert_eq!(substream_channels(&mono), vec![1, 1, 1, 1]);

        let projection = AudioElementConfig::AmbisonicsProjection {
            output_channel_count: 4,
            substream_count: 3,
            coupled_substream_count: 1,
            demixing_matrix: vec![],
        };
        assert_eq!(substream_channels(&projection), vec![2, 1, 1]);
    }

    #[test]
    fn trimming_applied_on_finish() {
        let sub = SubstreamFrames {
            substream_id: 0,
            channels: 1,
            sample_rate: 48000,
            frames: vec![
                FramePcm {
                    samples: vec![1.0, 2.0, 3.0, 4.0],
                    trim_start: 2,
                    trim_end: 0,
                },
                FramePcm {
                    samples: vec![5.0, 6.0, 7.0, 8.0],
                    trim_start: 0,
                    trim_end: 1,
                },
            ],
        };
        assert_eq!(sub.trimmed().samples, vec![3.0, 4.0, 5.0, 6.0, 7.0]);
    }
}
