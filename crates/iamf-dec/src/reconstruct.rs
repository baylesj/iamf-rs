//! Element reconstruction: decoded substream PCM → planar element channels
//! ready for rendering.
//!
//! Channel-based elements come out in rendering (channel_layout) order;
//! scene-based elements come out as ACN-ordered ambisonics channels.

use iamf_obu::descriptors::{AudioElement, AudioElementConfig};

use crate::element::SubstreamPcm;
use crate::layout::loudspeaker_info;
use crate::matrices::{HoaOrder, MatrixLayout};
use crate::DecodeError;

/// Planar element audio, one `Vec<f32>` per channel.
pub enum Reconstructed {
    Channels {
        matrix: MatrixLayout,
        planar: Vec<Vec<f32>>,
    },
    Hoa {
        order: HoaOrder,
        planar: Vec<Vec<f32>>,
    },
}

impl Reconstructed {
    pub fn planar(&self) -> &[Vec<f32>] {
        match self {
            Reconstructed::Channels { planar, .. } | Reconstructed::Hoa { planar, .. } => planar,
        }
    }
}

/// Splits each substream's interleaved PCM into per-channel planes, in
/// substream order (a coupled substream contributes two planes).
fn deinterleave(substreams: &[SubstreamPcm]) -> Vec<Vec<f32>> {
    let mut planes = Vec::new();
    for sub in substreams {
        let ch = usize::from(sub.channels.max(1));
        for c in 0..ch {
            planes.push(sub.samples.iter().skip(c).step_by(ch).copied().collect());
        }
    }
    planes
}

pub fn reconstruct(
    element: &AudioElement,
    substreams: &[SubstreamPcm],
) -> Result<Reconstructed, DecodeError> {
    let decoded = deinterleave(substreams);
    match &element.config {
        AudioElementConfig::ChannelBased { layers } => {
            let [layer] = layers.as_slice() else {
                return Err(DecodeError::Unimplemented(
                    "scalable channel audio (multi-layer demixing)",
                ));
            };
            if layer.output_gain.is_some() {
                return Err(DecodeError::Unimplemented("layer output_gain"));
            }
            let info = loudspeaker_info(layer.loudspeaker_layout).ok_or(
                DecodeError::InvalidDescriptors(format!(
                    "unsupported loudspeaker_layout {}",
                    layer.loudspeaker_layout
                )),
            )?;
            if decoded.len() != info.channels {
                return Err(DecodeError::InvalidDescriptors(format!(
                    "layout {} expects {} channels, substreams carry {}",
                    layer.loudspeaker_layout,
                    info.channels,
                    decoded.len()
                )));
            }
            let mut planar = vec![Vec::new(); info.channels];
            for (i, plane) in decoded.into_iter().enumerate() {
                planar[info.decoding_map[i]] = plane;
            }
            Ok(Reconstructed::Channels {
                matrix: info.matrix,
                planar,
            })
        }
        AudioElementConfig::AmbisonicsMono {
            output_channel_count,
            channel_mapping,
            ..
        } => {
            let order = hoa_order(*output_channel_count)?;
            let frames = decoded.first().map(Vec::len).unwrap_or(0);
            let planar = channel_mapping
                .iter()
                .map(|&m| {
                    if m == 255 {
                        Ok(vec![0.0; frames])
                    } else {
                        decoded.get(usize::from(m)).cloned().ok_or_else(|| {
                            DecodeError::InvalidDescriptors(format!(
                                "channel_mapping index {m} out of range"
                            ))
                        })
                    }
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Reconstructed::Hoa { order, planar })
        }
        AudioElementConfig::AmbisonicsProjection {
            output_channel_count,
            demixing_matrix,
            ..
        } => {
            let order = hoa_order(*output_channel_count)?;
            let rows = usize::from(*output_channel_count);
            let cols = decoded.len();
            if demixing_matrix.len() != rows * cols {
                return Err(DecodeError::InvalidDescriptors(format!(
                    "demixing matrix is {} entries, expected {rows}x{cols}",
                    demixing_matrix.len()
                )));
            }
            let frames = decoded.first().map(Vec::len).unwrap_or(0);
            // out[acn] = sum_l in[l] * matrix[l * rows + acn], Q1.15.
            let planar = (0..rows)
                .map(|r| {
                    (0..frames)
                        .map(|s| {
                            (0..cols)
                                .map(|l| {
                                    decoded[l][s]
                                        * (f32::from(demixing_matrix[l * rows + r]) / 32768.0)
                                })
                                .sum()
                        })
                        .collect()
                })
                .collect();
            Ok(Reconstructed::Hoa { order, planar })
        }
    }
}

fn hoa_order(channels: u8) -> Result<HoaOrder, DecodeError> {
    Ok(match channels {
        1 => HoaOrder::Zoa,
        4 => HoaOrder::Foa,
        9 => HoaOrder::Soa,
        16 => HoaOrder::Toa,
        25 => HoaOrder::H4a,
        _ => {
            return Err(DecodeError::InvalidDescriptors(format!(
                "{channels} channels is not a full ambisonics order"
            )))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use iamf_obu::descriptors::ChannelAudioLayer;

    fn sub(id: u32, channels: u8, samples: Vec<f32>) -> SubstreamPcm {
        SubstreamPcm {
            substream_id: id,
            channels,
            sample_rate: 48000,
            samples,
        }
    }

    #[test]
    fn stereo_passthrough_order() {
        let element = AudioElement {
            audio_element_id: 1,
            codec_config_id: 0,
            substream_ids: vec![0],
            params: vec![],
            config: AudioElementConfig::ChannelBased {
                layers: vec![ChannelAudioLayer {
                    loudspeaker_layout: 1,
                    substream_count: 1,
                    coupled_substream_count: 1,
                    recon_gain_is_present: false,
                    output_gain: None,
                    expanded_loudspeaker_layout: None,
                }],
            },
        };
        // Interleaved L/R.
        let subs = [sub(0, 2, vec![1.0, -1.0, 2.0, -2.0])];
        let rec = reconstruct(&element, &subs).unwrap();
        assert_eq!(rec.planar()[0], vec![1.0, 2.0]); // L
        assert_eq!(rec.planar()[1], vec![-1.0, -2.0]); // R
    }

    #[test]
    fn surround_decoding_map() {
        // 5.1: decode order L/R, Ls/Rs, C, LFE -> rendering order
        // L, R, C, LFE, Ls, Rs.
        let element = AudioElement {
            audio_element_id: 1,
            codec_config_id: 0,
            substream_ids: vec![0, 1, 2, 3],
            params: vec![],
            config: AudioElementConfig::ChannelBased {
                layers: vec![ChannelAudioLayer {
                    loudspeaker_layout: 2,
                    substream_count: 4,
                    coupled_substream_count: 2,
                    recon_gain_is_present: false,
                    output_gain: None,
                    expanded_loudspeaker_layout: None,
                }],
            },
        };
        let subs = [
            sub(0, 2, vec![1.0, 2.0]), // L, R
            sub(1, 2, vec![5.0, 6.0]), // Ls, Rs
            sub(2, 1, vec![3.0]),      // C
            sub(3, 1, vec![4.0]),      // LFE
        ];
        let rec = reconstruct(&element, &subs).unwrap();
        let flat: Vec<f32> = rec.planar().iter().map(|p| p[0]).collect();
        assert_eq!(flat, vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn ambisonics_mono_mapping() {
        let element = AudioElement {
            audio_element_id: 1,
            codec_config_id: 0,
            substream_ids: vec![0, 1],
            params: vec![],
            config: AudioElementConfig::AmbisonicsMono {
                output_channel_count: 4,
                substream_count: 2,
                channel_mapping: vec![1, 0, 255, 255],
            },
        };
        let subs = [sub(0, 1, vec![1.0]), sub(1, 1, vec![2.0])];
        let rec = reconstruct(&element, &subs).unwrap();
        let flat: Vec<f32> = rec.planar().iter().map(|p| p[0]).collect();
        assert_eq!(flat, vec![2.0, 1.0, 0.0, 0.0]);
    }
}
