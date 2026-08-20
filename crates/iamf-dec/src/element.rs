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

/// Decoded PCM of one substream: interleaved f32, `channels` wide.
#[derive(Debug, Clone, Default)]
pub struct SubstreamPcm {
    pub substream_id: u32,
    pub channels: u8,
    pub sample_rate: u32,
    pub samples: Vec<f32>,
}

/// Decodes every substream of a single audio element, applying per-frame
/// trimming. Reconstruction/rendering (demixing, layout mapping) is not
/// performed here — output is raw substream PCM in `substream_ids` order.
pub struct ElementDecoder {
    substream_ids: Vec<u32>,
    decoders: Vec<Box<dyn SubstreamDecoder>>,
    outputs: Vec<SubstreamPcm>,
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
            .map(|(&id, &ch)| SubstreamPcm {
                substream_id: id,
                channels: ch,
                ..SubstreamPcm::default()
            })
            .collect();
        Ok(ElementDecoder {
            substream_ids: element.substream_ids.clone(),
            decoders,
            outputs,
            scratch: DecodedFrame::default(),
        })
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
        let channels = usize::from(self.scratch.channels.max(1));
        let frame_count = self.scratch.samples.len() / channels;
        let trim_start = (frame.num_samples_to_trim_at_start as usize).min(frame_count);
        let trim_end = (frame.num_samples_to_trim_at_end as usize).min(frame_count - trim_start);
        let kept =
            &self.scratch.samples[trim_start * channels..(frame_count - trim_end) * channels];

        out.samples.extend_from_slice(kept);
        out.channels = self.scratch.channels;
        out.sample_rate = self.scratch.sample_rate;
        Ok(true)
    }

    /// Finishes decoding and yields per-substream PCM.
    pub fn finish(self) -> Vec<SubstreamPcm> {
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
}
