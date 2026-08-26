//! Element reconstruction: decoded substream PCM → planar element channels
//! ready for rendering.
//!
//! Channel-based elements go through [`ChannelReconstructor`], which
//! handles both single-layer passthrough and scalable multi-layer demixing
//! frame by frame. Scene-based elements come out as ACN-ordered ambisonics
//! channels via [`reconstruct_ambisonics`].

use iamf_obu::descriptors::{AudioElement, AudioElementConfig, ChannelAudioLayer};

use crate::DecodeError;
use crate::channels::{
    Channel, default_recon_flags, new_channels, output_gain_channel, recon_channel_gains,
    rendering_channels,
};
use crate::demixer::Demixer;
use crate::element::SubstreamPcm;
use crate::layout::{SoundSystem, loudspeaker_info, loudspeaker_sound_system};
use crate::matrices::{HoaOrder, MatrixLayout};
use crate::params::{ReconGainLayers, q78_db_to_linear};

/// Planar element audio, one `Vec<f32>` per channel.
#[derive(Debug)]
#[non_exhaustive]
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

/// Frame-based reconstruction of a channel-based element up to the layer
/// selected for the playback layout (libiamf `iamf_stream_set_output_layout`
/// + `iamf_stream_scale_demixer_configure`).
#[derive(Debug)]
pub struct ChannelReconstructor {
    demixer: Demixer,
    /// Index of the selected layer.
    layer: usize,
    /// Loudspeaker layout of the selected layer.
    layout: u8,
    /// Total decoded channels for layers 0..=layer, in decode order.
    input_channels: usize,
    matrix: MatrixLayout,
}

impl ChannelReconstructor {
    pub fn new(layers: &[ChannelAudioLayer], target: SoundSystem) -> Result<Self, DecodeError> {
        Self::with_layer_selection(layers, target, false)
    }

    /// `force_highest` selects the top layer regardless of the target —
    /// libiamf's behavior for binaural output with
    /// `headphones_rendering_mode == 1`.
    pub fn with_layer_selection(
        layers: &[ChannelAudioLayer],
        target: SoundSystem,
        force_highest: bool,
    ) -> Result<Self, DecodeError> {
        if layers.is_empty() {
            return Err(DecodeError::InvalidDescriptors("no channel layers".into()));
        }
        for layer in layers {
            if layer.expanded_loudspeaker_layout.is_some() || layer.loudspeaker_layout > 8 {
                return Err(DecodeError::Unimplemented(
                    "expanded/binaural loudspeaker layouts",
                ));
            }
        }

        // Layer selection: exact sound-system match, else the first layer
        // with more channels than the playback layout, else the highest.
        // Binaural selects like stereo (libiamf's
        // `_get_sound_system_layout_instance`).
        let selection_target = if target == SoundSystem::Binaural {
            SoundSystem::A
        } else {
            target
        };
        let mut selected = layers.len() - 1;
        if !force_highest {
            let exact = layers.iter().position(|l| {
                loudspeaker_sound_system(l.loudspeaker_layout) == Some(selection_target)
            });
            if let Some(i) = exact {
                selected = i;
            } else if layers.len() > 1 {
                let playback_channels = target.channels();
                let bigger = layers.iter().position(|l| {
                    rendering_channels(l.loudspeaker_layout).map_or(0, <[Channel]>::len)
                        > playback_channels
                });
                if let Some(i) = bigger {
                    selected = i;
                }
            }
        }

        // Decode-order channels and output gains, accumulated over layers
        // 0..=selected.
        let mut channels_in: Vec<Channel> = Vec::new();
        let mut output_gains = Vec::new();
        let mut last: Option<u8> = None;
        for layer in &layers[..=selected] {
            channels_in.extend(new_channels(last, layer.loudspeaker_layout));
            if let Some((flags, gain_q78)) = layer.output_gain {
                let gain = q78_db_to_linear(gain_q78);
                for bit in 0..6u8 {
                    if flags & (1 << bit) != 0 {
                        if let Some(ch) = output_gain_channel(layer.loudspeaker_layout, bit) {
                            output_gains.push((ch, gain));
                        }
                    }
                }
            }
            last = Some(layer.loudspeaker_layout);
        }

        let layout = layers[selected].loudspeaker_layout;
        let channels_out = rendering_channels(layout)
            .ok_or(DecodeError::InvalidDescriptors("bad layout".into()))?
            .to_vec();
        let matrix = loudspeaker_info(layout)
            .ok_or(DecodeError::InvalidDescriptors("bad layout".into()))?
            .matrix;
        let input_channels = channels_in.len();
        let mut demixer = Demixer::new(channels_in, channels_out, output_gains);

        // Default recon gains: 1.0 for the channels reconstructed between
        // the first layer and the selected layer.
        if selected > 0 {
            let flags = default_recon_flags(layers[0].loudspeaker_layout, layout);
            let gains = vec![1.0f32; flags.count_ones() as usize];
            let pairs = recon_channel_gains(layout, flags, &gains);
            demixer.set_recon_gains(flags, pairs);
        }

        Ok(ChannelReconstructor {
            demixer,
            layer: selected,
            layout,
            input_channels,
            matrix,
        })
    }

    pub fn input_channels(&self) -> usize {
        self.input_channels
    }

    /// Input-side key into the rendering matrix tables for the selected
    /// layer.
    pub fn matrix(&self) -> MatrixLayout {
        self.matrix
    }

    /// Loudspeaker layout of the selected layer.
    pub fn layout(&self) -> u8 {
        self.layout
    }

    /// Updates the demixing mode from a demixing parameter block (dynamic
    /// path: rotates state and steps the w index).
    pub fn set_demixing_mode(&mut self, mode: u8) -> Result<(), DecodeError> {
        self.demixer.set_demixing_info(mode, -1)
    }

    /// Sets the default demixing info from the element's parameter
    /// definition (static path).
    pub fn set_default_demixing(&mut self, mode: u8, w_idx: u8) -> Result<(), DecodeError> {
        self.demixer.set_demixing_info(mode, i32::from(w_idx))
    }

    /// Updates recon gains from a recon-gain parameter block: uses the
    /// selected layer's entry, gains are byte/255.
    pub fn set_recon_gains(&mut self, layers: &ReconGainLayers) {
        if let Some(Some((flags, gains))) = layers.get(self.layer) {
            let linear: Vec<f32> = gains.iter().map(|&g| f32::from(g) / 255.0).collect();
            let pairs = recon_channel_gains(self.layout, *flags, &linear);
            self.demixer.set_recon_gains(*flags, pairs);
        }
    }

    /// Demixes one frame of planes in decode order (only the first
    /// `input_channels` planes are used, so callers can pass all decoded
    /// channels even when a lower layer was selected).
    pub fn process_frame(&mut self, planes: &[Vec<f32>]) -> Result<Vec<Vec<f32>>, DecodeError> {
        if planes.len() < self.input_channels {
            return Err(DecodeError::InvalidDescriptors(format!(
                "expected {} decoded channels, got {}",
                self.input_channels,
                planes.len()
            )));
        }
        self.demixer.demix(&planes[..self.input_channels])
    }
}

/// Splits each substream's interleaved PCM into per-channel planes, in
/// substream order (a coupled substream contributes two planes).
pub fn deinterleave(samples: &[f32], channels: usize) -> Vec<Vec<f32>> {
    (0..channels)
        .map(|c| samples.iter().skip(c).step_by(channels).copied().collect())
        .collect()
}

/// ACN channel count → ambisonics order (√n − 1), saturating on empty
/// input so hostile plane lists cannot underflow.
pub(crate) fn hoa_order_index(channels: usize) -> usize {
    channels.isqrt().saturating_sub(1)
}

pub fn hoa_order(channels: u8) -> Result<HoaOrder, DecodeError> {
    Ok(match channels {
        1 => HoaOrder::Zoa,
        4 => HoaOrder::Foa,
        9 => HoaOrder::Soa,
        16 => HoaOrder::Toa,
        25 => HoaOrder::H4a,
        _ => {
            return Err(DecodeError::InvalidDescriptors(format!(
                "{channels} channels is not a full ambisonics order"
            )));
        }
    })
}

/// Reconstructs a scene-based element from trimmed substream PCM.
pub fn reconstruct_ambisonics(
    element: &AudioElement,
    substreams: &[SubstreamPcm],
) -> Result<Reconstructed, DecodeError> {
    let mut decoded = Vec::new();
    for sub in substreams {
        decoded.extend(deinterleave(&sub.samples, usize::from(sub.channels.max(1))));
    }
    ambisonics_from_planes(&element.config, decoded)
}

/// Converts decoded substream planes (decode order) of a scene-based
/// element into ACN-ordered ambisonics channels. Stateless, so it works
/// per frame or on whole buffers.
pub fn ambisonics_from_planes(
    config: &AudioElementConfig,
    decoded: Vec<Vec<f32>>,
) -> Result<Reconstructed, DecodeError> {
    match config {
        AudioElementConfig::AmbisonicsMono {
            output_channel_count,
            channel_mapping,
            ..
        } => {
            let order = hoa_order(*output_channel_count)?;
            let frames = decoded.first().map_or(0, Vec::len);
            // Each decoded plane is moved to its ACN slot; §3.7.5 requires
            // the mapping to be injective, so a repeated index is invalid.
            let mut decoded: Vec<Option<Vec<f32>>> = decoded.into_iter().map(Some).collect();
            let planar = channel_mapping
                .iter()
                .map(|&m| {
                    if m == 255 {
                        Ok(vec![0.0; frames])
                    } else {
                        decoded
                            .get_mut(usize::from(m))
                            .and_then(Option::take)
                            .ok_or_else(|| {
                                DecodeError::InvalidDescriptors(format!(
                                    "channel_mapping index {m} out of range or repeated"
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
            let frames = decoded.first().map_or(0, Vec::len);
            // out[acn] = sum_l in[l] * matrix[l * rows + acn], Q1.15.
            // Iterated decoded-channel-major with the coefficient hoisted;
            // per-sample accumulation stays in l order (bit-identical to a
            // per-sample sum over l).
            let mut planar = vec![vec![0.0f32; frames]; rows];
            for (l, plane) in decoded.iter().enumerate() {
                for (r, out) in planar.iter_mut().enumerate() {
                    let coefficient = f32::from(demixing_matrix[l * rows + r]) / 32768.0;
                    for (o, &sample) in out.iter_mut().zip(plane) {
                        *o += sample * coefficient;
                    }
                }
            }
            Ok(Reconstructed::Hoa { order, planar })
        }
        AudioElementConfig::ChannelBased { .. } => Err(DecodeError::InvalidDescriptors(
            "channel-based element in ambisonics reconstruction".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iamf_obu::descriptors::ChannelAudioLayer;

    fn layer(layout: u8, substreams: u8, coupled: u8) -> ChannelAudioLayer {
        ChannelAudioLayer {
            loudspeaker_layout: layout,
            substream_count: substreams,
            coupled_substream_count: coupled,
            recon_gain_is_present: false,
            output_gain: None,
            expanded_loudspeaker_layout: None,
        }
    }

    #[test]
    fn projection_applies_q15_matrix() {
        // FOA from 3 substreams (1 coupled + 2 mono = 4 decoded channels).
        // Matrix rows are decoded channels, columns ACN outputs, Q1.15.
        let q = |x: f32| (x * 32768.0) as i16;
        #[rustfmt::skip]
        let matrix = vec![
            // decoded 0 -> W only; decoded 1 -> Y; 2 -> Z; 3 -> X + half W.
            q(0.5), q(0.0), q(0.0),  q(0.0),
            q(0.0), q(0.9), q(0.0),  q(0.0),
            q(0.0), q(0.0), q(-0.5), q(0.0),
            q(0.25), q(0.0), q(0.0), q(0.99),
        ];
        let config = AudioElementConfig::AmbisonicsProjection {
            output_channel_count: 4,
            substream_count: 3,
            coupled_substream_count: 1,
            demixing_matrix: matrix,
        };
        let decoded = vec![vec![1.0f32], vec![0.5], vec![-1.0], vec![0.8]];
        let out = ambisonics_from_planes(&config, decoded).unwrap();
        let Reconstructed::Hoa { order, planar } = out else {
            panic!("expected hoa");
        };
        assert_eq!(order, HoaOrder::Foa);
        let tol = 1.0 / 32768.0;
        assert!((planar[0][0] - (0.5 + 0.25 * 0.8)).abs() <= tol, "W");
        assert!((planar[1][0] - 0.45).abs() <= tol, "Y");
        assert!((planar[2][0] - 0.5).abs() <= tol, "Z");
        assert!((planar[3][0] - 0.99 * 0.8).abs() <= tol * 2.0, "X");
    }

    #[test]
    fn projection_rejects_wrong_matrix_size() {
        let config = AudioElementConfig::AmbisonicsProjection {
            output_channel_count: 4,
            substream_count: 3,
            coupled_substream_count: 1,
            demixing_matrix: vec![0; 12], // needs 4x4 = 16
        };
        assert!(ambisonics_from_planes(&config, vec![vec![0.0]; 4]).is_err());
    }

    #[test]
    fn single_layer_stereo_passthrough() {
        let layers = [layer(1, 1, 1)];
        let mut rec = ChannelReconstructor::new(&layers, SoundSystem::A).unwrap();
        assert_eq!(rec.input_channels(), 2);
        let out = rec
            .process_frame(&[vec![1.0, 2.0], vec![-1.0, -2.0]])
            .unwrap();
        assert_eq!(out[0], vec![1.0, 2.0]);
        assert_eq!(out[1], vec![-1.0, -2.0]);
    }

    #[test]
    fn single_layer_51_reorders_to_rendering() {
        // Decode order: L/R, Ls/Rs, C, LFE -> rendering L,R,C,LFE,Ls,Rs.
        let layers = [layer(2, 4, 2)];
        let mut rec = ChannelReconstructor::new(&layers, SoundSystem::B).unwrap();
        let planes: Vec<Vec<f32>> = [1.0, 2.0, 5.0, 6.0, 3.0, 4.0]
            .iter()
            .map(|&v| vec![v])
            .collect();
        let out = rec.process_frame(&planes).unwrap();
        let flat: Vec<f32> = out.iter().map(|p| p[0]).collect();
        assert_eq!(flat, vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn scalable_selects_matching_layer() {
        // Stereo + 5.1 rendered to stereo: only the stereo layer is used.
        let layers = [layer(1, 1, 1), layer(2, 3, 1)];
        let mut rec = ChannelReconstructor::new(&layers, SoundSystem::A).unwrap();
        assert_eq!(rec.input_channels(), 2);
        assert_eq!(rec.matrix, MatrixLayout::Stereo);
        let out = rec.process_frame(&[vec![0.5], vec![0.25]]).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0][0], 0.5);
    }

    #[test]
    fn scalable_demixes_higher_layer() {
        // Stereo + 5.1 rendered to 5.1: Ls/Rs are demixed.
        let layers = [layer(1, 1, 1), layer(2, 3, 1)];
        let mut rec = ChannelReconstructor::new(&layers, SoundSystem::B).unwrap();
        assert_eq!(rec.input_channels(), 6);
        // channels_in: L2, R2, L5, R5, C, LFE.
        let ls = 0.4f32;
        let l5 = 0.1f32;
        let c = 0.2f32;
        let l3 = l5 + 0.707 * ls;
        let l2 = l3 + 0.707 * c;
        let planes: Vec<Vec<f32>> = [l2, 0.0, l5, 0.0, c, 0.0]
            .iter()
            .map(|&v| vec![v])
            .collect();
        let out = rec.process_frame(&planes).unwrap();
        // Default recon gain smoothing on first frame: 0.25*1 + 0.75*1 = 1.
        assert!((out[4][0] - ls).abs() < 1e-5, "Ls = {}", out[4][0]);
    }
}
