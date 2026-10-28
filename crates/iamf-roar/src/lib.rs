//! ROAR as an external renderer for the iamf-dec streaming decoder.
//!
//! [ROAR](https://github.com/AOMediaCodec/roar) is AOM's Rust port of the
//! Open Audio Renderer (OAR), the rendering algorithm IAMF v2 references.
//! [`RoarRendererFactory`] implements iamf-dec's
//! [`MixRendererFactory`] hook, so a
//! [`StreamDecoder`](iamf_dec::stream::StreamDecoder) created with
//! [`new_from_descriptors_with_renderer`](iamf_dec::stream::StreamDecoder::new_from_descriptors_with_renderer)
//! renders through ROAR instead of its builtin renderer. Decoding,
//! reconstruction, parameter timing, gains, loudness and the limiter stay
//! in iamf-dec.
//!
//! Mapping (see also `ARCHITECTURE.md`):
//!
//! - **Target layout**: sound systems A–J and the IAMF extensions map to the
//!   ROAR `Layout` with the same channel order (ROAR's EAR tables share
//!   libiamf's channel conventions); binaural maps to `Layout::Binaural`.
//! - **Channel-based element** → `ChannelBased` with the loudspeaker layout
//!   of the layer the decoder reconstructed. Elements carrying a demixing
//!   parameter definition get `DownmixInfo` with their default
//!   `dmixp_mode`, and each temporal unit's demixing mode is forwarded, so
//!   ROAR selects OAR's IAMF downmix renderer where the layout pair
//!   supports it (EAR otherwise). ROAR does not export its weight-index
//!   type, so the initial `w_idx` cannot be passed: ROAR derives it from
//!   the mode's shift direction instead.
//! - **Binaural-input element** (loudspeaker_layout BINAURAL) → stereo
//!   `ChannelBased` rendered with `WorldLockedRestricted`, i.e. passed
//!   through to the stereo or binaural output.
//! - **Scene-based element** → `SceneBased` of the element's order.
//! - **headphones_rendering_mode** (binaural target only): 0 → stereo
//!   (`WorldLockedRestricted`), 1 → `WorldLocked`, 2 → `HeadLocked`. For 1
//!   and 2 the decoder hands over the element's highest layer. ROAR routes
//!   *every* scene-based element through OBR regardless of this mode (only
//!   channel-based elements honor the stereo fallback).
//! - **Gains**: element mix gains arrive already applied to each element's
//!   planes (ROAR's renderers are linear, so this equals ROAR's
//!   post-render element gain). The output mix gain is applied by the
//!   decoder after rendering.
//!
//! ROAR renders fixed-size blocks chosen at creation: the codec frame size
//! (`num_samples_per_frame`), or its largest divisor within ROAR's
//! 16384-sample limit. A unit that is not a whole number of blocks has its
//! last block zero-padded (and the padding discarded), like the builtin
//! binaural renderer.
//!
//! Known upstream issue at the pinned ROAR revision: scene-based elements
//! rendered to sound system H (22.2) get a wrong output channel 10 (see
//! `ARCHITECTURE.md`). [`RoarRendererFactory`] refuses that combination
//! with [`DecodeError::UnsupportedRenderer`].

use aom_roar::{
    AudioElementConfig, BinauralFilterProfile, ChannelBasedConfig, Config, DownmixInfo,
    DownmixMode, ElementRenderingConfig, GroupId, HeadphonesRenderingMode, HighOrderAmbisonics,
    Layout, OarError, ObjectBasedConfig, ObjectPosition, PlanarBufferMut, PlanarBufferRef,
    PolarCoordinate, RoarRenderer, SampleRate, Samples, SceneBasedConfig,
};

use iamf_dec::DecodeError;
use iamf_dec::layout::{
    LOUDSPEAKER_LAYOUT_7_1_5_4, LOUDSPEAKER_LAYOUT_9_1_6, LOUDSPEAKER_LAYOUT_10_2_9_3, SoundSystem,
};
use iamf_dec::objects::PolarCoord;
use iamf_dec::renderer::{
    BinauralFilterProfile as IamfFilterProfile, ElementFrame, ElementLayout, MixConfig,
    MixRenderer, MixRendererFactory,
};

/// ROAR's maximum block size (`MAX_SAMPLES_PER_CHANNEL`).
const MAX_BLOCK: usize = 16384;

/// IAMF `loudspeaker_layout` of pre-rendered binaural input (§3.6.2).
const LOUDSPEAKER_LAYOUT_BINAURAL: u8 = 9;

/// Creates ROAR-backed [`MixRenderer`]s for iamf-dec's streaming decoder
/// (see the [crate docs](crate)).
#[derive(Debug, Clone, Copy, Default)]
#[non_exhaustive]
pub struct RoarRendererFactory;

impl RoarRendererFactory {
    /// A factory with ROAR's default settings.
    pub fn new() -> Self {
        RoarRendererFactory
    }
}

impl MixRendererFactory for RoarRendererFactory {
    fn create(&self, config: &MixConfig<'_>) -> Result<Box<dyn MixRenderer>, DecodeError> {
        let specs = config
            .elements
            .iter()
            .map(|info| {
                let kind = match info.layout {
                    ElementLayout::Channels { loudspeaker_layout } => ElementKind::Channels {
                        loudspeaker_layout,
                        default_dmixp_mode: info.default_demixing_mode,
                    },
                    ElementLayout::Ambisonics { order } => ElementKind::Hoa { order },
                    ElementLayout::Objects { num_objects } => ElementKind::Objects { num_objects },
                    other => {
                        return Err(DecodeError::UnsupportedRenderer(format!(
                            "ROAR adapter does not handle element layout {other:?}"
                        )));
                    }
                };
                Ok(ElementSpec {
                    id: info.audio_element_id,
                    kind,
                    headphones_rendering_mode: info.headphones_rendering_mode,
                    binaural_filter_profile: info.binaural_filter_profile,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Box::new(RoarMix::new(
            config.target,
            config.frame_size as usize,
            config.sample_rate,
            &specs,
        )?))
    }
}

impl MixRenderer for RoarMix {
    fn render(&mut self, elements: &[ElementFrame<'_>]) -> Result<Vec<Vec<f32>>, DecodeError> {
        let inputs: Vec<UnitInput<'_>> = elements
            .iter()
            .map(|e| UnitInput {
                id: e.audio_element_id,
                dmixp_mode: e.demixing_mode,
                object_positions: e.object_positions,
                planar: e.planes,
            })
            .collect();
        self.render_unit(&inputs)
    }
}

fn roar_error(context: &str, err: OarError) -> DecodeError {
    DecodeError::UnsupportedRenderer(format!("ROAR {context}: {err}"))
}

/// The ROAR output layout for a target sound system.
fn target_layout(target: SoundSystem) -> Option<Layout> {
    Some(match target {
        SoundSystem::A => Layout::Stereo,
        SoundSystem::B => Layout::Layout51,
        SoundSystem::C => Layout::Layout512,
        SoundSystem::D => Layout::Layout514,
        SoundSystem::E => Layout::SoundSystemE451,
        SoundSystem::F => Layout::SoundSystemF370,
        SoundSystem::G => Layout::SoundSystemG490,
        SoundSystem::H => Layout::LayoutA293,
        SoundSystem::I => Layout::Layout71,
        SoundSystem::J => Layout::Layout714,
        SoundSystem::Ext712 => Layout::Layout712,
        SoundSystem::Ext312 => Layout::Layout312,
        SoundSystem::Mono => Layout::Mono,
        SoundSystem::Ext916 => Layout::Layout916,
        SoundSystem::Binaural => Layout::Binaural,
        SoundSystem::Ext7154 => Layout::Layout7154,
        _ => return None,
    })
}

/// The ROAR input layout for an IAMF `loudspeaker_layout` (0..=8,
/// expanded parent layouts, and binaural input as stereo).
fn input_layout(loudspeaker_layout: u8) -> Option<Layout> {
    Some(match loudspeaker_layout {
        0 => Layout::Mono,
        1 | LOUDSPEAKER_LAYOUT_BINAURAL => Layout::Stereo,
        2 => Layout::Layout51,
        3 => Layout::Layout512,
        4 => Layout::Layout514,
        5 => Layout::Layout71,
        6 => Layout::Layout712,
        7 => Layout::Layout714,
        8 => Layout::Layout312,
        LOUDSPEAKER_LAYOUT_9_1_6 => Layout::Layout916,
        LOUDSPEAKER_LAYOUT_10_2_9_3 => Layout::LayoutA293,
        LOUDSPEAKER_LAYOUT_7_1_5_4 => Layout::Layout7154,
        _ => return None,
    })
}

/// IAMF `dmixp_mode` (§3.8.1) → ROAR downmix mode; reserved modes → `None`.
fn downmix_mode(dmixp_mode: u8) -> Option<DownmixMode> {
    Some(match dmixp_mode {
        0 => DownmixMode::Mode1NegOffset,
        1 => DownmixMode::Mode2NegOffset,
        2 => DownmixMode::Mode3NegOffset,
        4 => DownmixMode::Mode1PosOffset,
        5 => DownmixMode::Mode2PosOffset,
        6 => DownmixMode::Mode3PosOffset,
        _ => return None,
    })
}

fn hoa_order(order: u8) -> Option<HighOrderAmbisonics> {
    Some(match order {
        0 => HighOrderAmbisonics::Zoa,
        1 => HighOrderAmbisonics::Order1,
        2 => HighOrderAmbisonics::Order2,
        3 => HighOrderAmbisonics::Order3,
        4 => HighOrderAmbisonics::Order4,
        _ => return None,
    })
}

/// Largest block size ≤ ROAR's limit that divides `frame_len`.
fn block_size(frame_len: usize) -> usize {
    if frame_len <= MAX_BLOCK {
        return frame_len.max(1);
    }
    (1..=MAX_BLOCK)
        .rev()
        .find(|&b| frame_len.is_multiple_of(b))
        .unwrap_or(MAX_BLOCK)
}

/// What kind of element ROAR is rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ElementKind {
    /// Channel-based: the reconstructed layer's `loudspeaker_layout` and,
    /// for elements with a demixing parameter, the default `dmixp_mode`.
    Channels {
        loudspeaker_layout: u8,
        default_dmixp_mode: Option<u8>,
    },
    /// Scene-based, ACN-ordered planes of the given order.
    Hoa { order: u8 },
    /// Object-based, `num_objects` mono planes.
    Objects { num_objects: u8 },
}

/// One element of the mix, as configured at creation.
#[derive(Debug, Clone, Copy)]
struct ElementSpec {
    /// IAMF `audio_element_id` (also the ROAR element id).
    id: u32,
    kind: ElementKind,
    /// §3.8.2 headphones_rendering_mode (binaural target only).
    headphones_rendering_mode: u8,
    /// §3.8.2 binaural_filter_profile (binaural target only).
    binaural_filter_profile: IamfFilterProfile,
}

/// One element's audio for one temporal unit (mix gain already applied).
#[derive(Debug, Clone, Copy)]
struct UnitInput<'a> {
    id: u32,
    /// Demixing mode that applies to this unit, if a parameter block
    /// covers it.
    dmixp_mode: Option<u8>,
    /// Polar positions for each object of an object-based element.
    object_positions: &'a [PolarCoord],
    /// Planar element channels, one plane per channel, all unit-length.
    planar: &'a [Vec<f32>],
}

#[derive(Debug)]
struct ElementState {
    id: u32,
    channels: usize,
    /// Whether ROAR instantiated its downmix renderer for this element
    /// (dynamic demixing-mode updates are only valid then).
    downmix_active: bool,
}

/// A configured ROAR instance for one sub-mix and output layout.
struct RoarMix {
    renderer: RoarRenderer,
    block: usize,
    out_channels: usize,
    elements: Vec<ElementState>,
    /// Per-channel output scratch, one block each.
    scratch: Vec<Vec<f32>>,
}

impl core::fmt::Debug for RoarMix {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RoarMix")
            .field("block", &self.block)
            .field("out_channels", &self.out_channels)
            .field("elements", &self.elements)
            .finish_non_exhaustive()
    }
}

impl RoarMix {
    /// Creates a ROAR instance for the elements `specs` rendering units of
    /// nominally `frame_len` samples.
    fn new(
        target: SoundSystem,
        frame_len: usize,
        sample_rate: u32,
        specs: &[ElementSpec],
    ) -> Result<Self, DecodeError> {
        // Upstream ROAR bug at the pinned revision: the ambisonics to
        // channel renderer re-inserts the two 22.2 LFE slots by comparing
        // the LFE index with the source (not destination) channel index,
        // so output channel 10 is wrong. Refuse until ROAR is fixed.
        if target == SoundSystem::H
            && specs
                .iter()
                .any(|s| matches!(s.kind, ElementKind::Hoa { .. }))
        {
            return Err(DecodeError::UnsupportedRenderer(
                "ROAR renders scene-based elements to sound system H incorrectly \
                 (upstream LFE re-insertion bug); use the builtin renderer"
                    .into(),
            ));
        }
        let layout = target_layout(target).ok_or_else(|| {
            DecodeError::UnsupportedRenderer(format!("ROAR has no output layout for {target:?}"))
        })?;
        let block = block_size(frame_len);
        let config = Config::new(
            layout,
            Samples::new(block as u32).map_err(|e| roar_error("block size", e))?,
            SampleRate::new(sample_rate).map_err(|e| roar_error("sample rate", e))?,
        )
        .map_err(|e| roar_error("config", e))?;
        let mut renderer = RoarRenderer::create(&config).map_err(|e| roar_error("create", e))?;
        let group: GroupId = renderer
            .add_audio_group()
            .map_err(|e| roar_error("add group", e))?;

        let binaural = target == SoundSystem::Binaural;
        let mut elements = Vec::with_capacity(specs.len());
        for spec in specs {
            let passthrough = matches!(
                spec.kind,
                ElementKind::Channels {
                    loudspeaker_layout: LOUDSPEAKER_LAYOUT_BINAURAL,
                    ..
                }
            );
            let rendering_config = binaural.then_some(ElementRenderingConfig {
                headphones_rendering_mode: match spec.headphones_rendering_mode {
                    _ if passthrough => HeadphonesRenderingMode::WorldLockedRestricted,
                    1 => HeadphonesRenderingMode::WorldLocked,
                    2 => HeadphonesRenderingMode::HeadLocked,
                    _ => HeadphonesRenderingMode::WorldLockedRestricted,
                },
                binaural_filter_profile: match spec.binaural_filter_profile {
                    IamfFilterProfile::Direct => BinauralFilterProfile::Direct,
                    IamfFilterProfile::Reverberant => BinauralFilterProfile::Reverberant,
                    _ => BinauralFilterProfile::Ambient,
                },
            });
            let (element_config, downmix_active) = match spec.kind {
                ElementKind::Channels {
                    loudspeaker_layout,
                    default_dmixp_mode,
                } => {
                    let in_layout = input_layout(loudspeaker_layout).ok_or_else(|| {
                        DecodeError::UnsupportedRenderer(format!(
                            "ROAR has no input layout for loudspeaker_layout {loudspeaker_layout}"
                        ))
                    })?;
                    let downmix_info = default_dmixp_mode
                        .and_then(downmix_mode)
                        .map(|mode| DownmixInfo::new(mode, None));
                    let downmix_active = downmix_info.is_some()
                        && !binaural
                        && in_layout.is_valid_downmix_to(layout);
                    (
                        AudioElementConfig::ChannelBased(ChannelBasedConfig {
                            layout: in_layout,
                            downmix_info,
                            rendering_config,
                        }),
                        downmix_active,
                    )
                }
                ElementKind::Hoa { order } => {
                    let order = hoa_order(order).ok_or_else(|| {
                        DecodeError::UnsupportedRenderer(format!(
                            "ROAR has no ambisonics order {order}"
                        ))
                    })?;
                    (
                        AudioElementConfig::SceneBased(SceneBasedConfig {
                            order,
                            rendering_config,
                        }),
                        false,
                    )
                }
                ElementKind::Objects { num_objects } => (
                    AudioElementConfig::ObjectBased(ObjectBasedConfig {
                        num_objects: u32::from(num_objects),
                        rendering_config,
                    }),
                    false,
                ),
            };
            renderer
                .add_element(group, spec.id, &element_config)
                .map_err(|e| roar_error("add element", e))?;
            elements.push(ElementState {
                id: spec.id,
                channels: element_config.channels(),
                downmix_active,
            });
        }
        let out_channels = layout.channels();
        Ok(RoarMix {
            renderer,
            block,
            out_channels,
            elements,
            scratch: vec![vec![0.0; block]; out_channels],
        })
    }

    /// Renders one temporal unit of all elements, returning planar output
    /// channels of the unit's length. `inputs` must describe the same
    /// elements, in the same order, as at creation.
    fn render_unit(&mut self, inputs: &[UnitInput<'_>]) -> Result<Vec<Vec<f32>>, DecodeError> {
        if inputs.len() != self.elements.len()
            || inputs
                .iter()
                .zip(&self.elements)
                .any(|(i, e)| i.id != e.id || i.planar.len() != e.channels)
        {
            return Err(DecodeError::InvalidDescriptors(
                "ROAR: element set changed mid-stream".into(),
            ));
        }
        for (input, state) in inputs.iter().zip(&self.elements) {
            if let Some(mode) = input.dmixp_mode.and_then(downmix_mode)
                && state.downmix_active
            {
                self.renderer
                    .update_element_downmix_mode(input.id, mode, None)
                    .map_err(|e| roar_error("downmix mode", e))?;
            }
        }

        let unit_len = inputs
            .first()
            .and_then(|i| i.planar.first())
            .map_or(0, Vec::len);
        let block = self.block;
        let samples = Samples::new(block as u32).map_err(|e| roar_error("block size", e))?;
        let mut out = vec![Vec::with_capacity(unit_len); self.out_channels];
        // Zero-padded copies for a trailing partial block.
        let mut padded: Vec<Vec<Vec<f32>>> = Vec::new();
        let mut pos = 0usize;
        while pos < unit_len {
            for input in inputs {
                if !input.object_positions.is_empty() {
                    let coords = input
                        .object_positions
                        .iter()
                        .map(|p| {
                            PolarCoordinate::new_from_floats(p.azimuth, p.elevation, p.distance)
                                .map_err(|e| roar_error("object position", e))
                        })
                        .collect::<Result<Vec<_>, _>>()?;
                    self.renderer
                        .update_element_positions(input.id, &ObjectPosition::Polar(coords), samples)
                        .map_err(|e| roar_error("update object positions", e))?;
                }
            }
            let n = block.min(unit_len - pos);
            if n < block {
                padded = inputs
                    .iter()
                    .map(|i| {
                        i.planar
                            .iter()
                            .map(|p| {
                                let mut c = p[pos..pos + n].to_vec();
                                c.resize(block, 0.0);
                                c
                            })
                            .collect()
                    })
                    .collect();
            }
            let channel_slices: Vec<Vec<&[f32]>> = if n < block {
                padded
                    .iter()
                    .map(|planes| planes.iter().map(Vec::as_slice).collect())
                    .collect()
            } else {
                inputs
                    .iter()
                    .map(|i| i.planar.iter().map(|p| &p[pos..pos + block]).collect())
                    .collect()
            };
            let mut refs = Vec::with_capacity(inputs.len());
            for (input, slices) in inputs.iter().zip(&channel_slices) {
                refs.push((
                    input.id,
                    PlanarBufferRef::new(slices, slices.len(), samples)
                        .map_err(|e| roar_error("input buffer", e))?,
                ));
            }
            let mut out_slices: Vec<&mut [f32]> =
                self.scratch.iter_mut().map(Vec::as_mut_slice).collect();
            let mut output = PlanarBufferMut::new(&mut out_slices, self.out_channels, samples)
                .map_err(|e| roar_error("output buffer", e))?;
            self.renderer
                .render(&refs, &mut output)
                .map_err(|e| roar_error("render", e))?;
            for (o, s) in out.iter_mut().zip(&self.scratch) {
                o.extend_from_slice(&s[..n]);
            }
            pos += n;
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channels_spec(id: u32, layout: u8) -> ElementSpec {
        ElementSpec {
            id,
            kind: ElementKind::Channels {
                loudspeaker_layout: layout,
                default_dmixp_mode: None,
            },
            headphones_rendering_mode: 0,
            binaural_filter_profile: IamfFilterProfile::default(),
        }
    }

    fn unit(id: u32, planar: &[Vec<f32>]) -> UnitInput<'_> {
        UnitInput {
            id,
            dmixp_mode: None,
            object_positions: &[],
            planar,
        }
    }

    #[test]
    fn block_sizes() {
        assert_eq!(block_size(960), 960);
        assert_eq!(block_size(16384), 16384);
        assert_eq!(block_size(48000), 16000);
        assert_eq!(block_size(0), 1);
    }

    #[test]
    fn target_channel_counts_match() {
        for n in 0..=14u8 {
            let target = SoundSystem::from_u8(n).unwrap();
            assert_eq!(
                target_layout(target).unwrap().channels(),
                target.channels(),
                "{target:?}"
            );
            let bs_target = SoundSystem::from_bitstream_sound_system(n).unwrap();
            assert_eq!(
                target_layout(bs_target).unwrap().channels(),
                bs_target.channels(),
                "{bs_target:?}"
            );
        }
    }

    #[test]
    fn expanded_parent_layouts_render() {
        for (layout_id, in_channels) in [
            (LOUDSPEAKER_LAYOUT_9_1_6, 16),
            (LOUDSPEAKER_LAYOUT_10_2_9_3, 24),
            (LOUDSPEAKER_LAYOUT_7_1_5_4, 17),
        ] {
            let planar = vec![vec![0.125f32; 64]; in_channels];
            for target in [SoundSystem::A, SoundSystem::Ext7154, SoundSystem::Binaural] {
                let mut mix =
                    RoarMix::new(target, 64, 48000, &[channels_spec(1, layout_id)]).unwrap();
                let out = mix.render_unit(&[unit(1, &planar)]).unwrap();
                assert_eq!(out.len(), target.channels());
                assert!(out.iter().all(|ch| ch.len() == 64));
                assert!(out.iter().flatten().any(|&s| s.abs() > 1e-4));
            }
        }
    }

    #[test]
    fn stereo_passthrough() {
        let planar = vec![vec![0.5f32; 64], vec![-0.25f32; 64]];
        let mut mix = RoarMix::new(SoundSystem::A, 64, 48000, &[channels_spec(7, 1)]).unwrap();
        let out = mix.render_unit(&[unit(7, &planar)]).unwrap();
        assert_eq!(out.len(), 2);
        for (o, i) in out.iter().zip(&planar) {
            for (a, b) in o.iter().zip(i) {
                assert!((a - b).abs() < 1e-6, "{a} vs {b}");
            }
        }
    }

    #[test]
    fn partial_blocks_are_padded() {
        // Created for 64-sample units, fed a 100-sample unit: rendered as
        // 64 + zero-padded 36.
        let planar = vec![vec![1.0f32; 100]; 2];
        let mut mix = RoarMix::new(SoundSystem::A, 64, 48000, &[channels_spec(1, 1)]).unwrap();
        let out = mix.render_unit(&[unit(1, &planar)]).unwrap();
        assert_eq!(out[0].len(), 100);
        assert!(out[0].iter().all(|&s| (s - 1.0).abs() < 1e-6));
    }

    /// The block size comes from the configured frame size, so an empty
    /// or short first unit does not shrink every later block.
    #[test]
    fn block_size_follows_frame_size_not_first_unit() {
        let mut mix = RoarMix::new(SoundSystem::A, 960, 48000, &[channels_spec(1, 1)]).unwrap();
        assert_eq!(mix.block, 960);
        let empty = vec![Vec::new(); 2];
        let out = mix.render_unit(&[unit(1, &empty)]).unwrap();
        assert!(out.iter().all(Vec::is_empty));
        let short = vec![vec![0.25f32; 10]; 2];
        assert_eq!(mix.render_unit(&[unit(1, &short)]).unwrap()[0].len(), 10);
        assert_eq!(mix.block, 960);
    }

    #[test]
    fn element_set_change_rejected() {
        let planar = vec![vec![0.0; 8]; 2];
        let mut mix = RoarMix::new(SoundSystem::A, 8, 48000, &[channels_spec(1, 1)]).unwrap();
        assert!(mix.render_unit(&[unit(2, &planar)]).is_err());
    }

    #[test]
    fn scene_based_to_sound_system_h_refused() {
        let hoa = ElementSpec {
            id: 1,
            kind: ElementKind::Hoa { order: 1 },
            headphones_rendering_mode: 0,
            binaural_filter_profile: IamfFilterProfile::default(),
        };
        assert!(matches!(
            RoarMix::new(SoundSystem::H, 960, 48000, &[hoa]),
            Err(DecodeError::UnsupportedRenderer(_))
        ));
        // Other targets, and channel-based input to H, are fine.
        assert!(RoarMix::new(SoundSystem::J, 960, 48000, &[hoa]).is_ok());
        assert!(RoarMix::new(SoundSystem::H, 960, 48000, &[channels_spec(1, 2)]).is_ok());
    }

    #[test]
    fn binaural_input_passes_through_to_binaural() {
        let planar = vec![vec![0.5f32; 64], vec![-0.25f32; 64]];
        let mut spec = channels_spec(3, LOUDSPEAKER_LAYOUT_BINAURAL);
        // Even with a binaural rendering mode, binaural input is not
        // binauralized again.
        spec.headphones_rendering_mode = 1;
        let mut mix = RoarMix::new(SoundSystem::Binaural, 64, 48000, &[spec]).unwrap();
        let out = mix.render_unit(&[unit(3, &planar)]).unwrap();
        for (o, i) in out.iter().zip(&planar) {
            for (a, b) in o.iter().zip(i) {
                assert!((a - b).abs() < 1e-6, "{a} vs {b}");
            }
        }
    }
}
