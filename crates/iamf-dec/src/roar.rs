//! ROAR renderer backend (`roar` feature): maps IAMF elements and target
//! layouts onto [ROAR](https://github.com/AOMediaCodec/roar), the Rust port
//! of AOM's Open Audio Renderer.
//!
//! Mapping (see also `ARCHITECTURE.md`):
//!
//! - **Target layout**: sound systems A–J and the IAMF extensions map to the
//!   ROAR `Layout` with the same channel order (ROAR's EAR tables share
//!   libiamf's channel conventions); binaural maps to `Layout::Binaural`.
//! - **Channel-based element** → `ChannelBased` with the loudspeaker layout
//!   of the layer the decoder reconstructed (same layer selection as the
//!   builtin path). Elements carrying a demixing parameter definition get
//!   `DownmixInfo` with their default `dmixp_mode`, and each temporal unit's
//!   demixing mode is forwarded, so ROAR selects OAR's IAMF downmix renderer
//!   where the layout pair supports it (EAR otherwise). ROAR does not
//!   export its weight-index type, so the initial `w_idx` cannot be passed:
//!   ROAR derives it from the mode's shift direction instead.
//! - **Scene-based element** → `SceneBased` with the order implied by the
//!   reconstructed ACN channel count.
//! - **headphones_rendering_mode** (binaural target only): 0 → stereo
//!   (`WorldLockedRestricted`), 1 → `WorldLocked`, 2 → `HeadLocked`. ROAR
//!   routes *every* scene-based element through OBR regardless of this mode
//!   (only channel-based elements honor the stereo fallback).
//! - **Sub-mixes** → ROAR audio groups (one per sub-mix). ROAR supports at
//!   most two groups; more is rejected with
//!   [`DecodeError::UnsupportedRenderer`] at configuration time.
//! - **Gains**: element mix gains are evaluated by the decoder (the same
//!   per-sample timeline, including libiamf's parameter-rate scaling, as the
//!   builtin path) and applied to each element's input before ROAR. ROAR's
//!   renderers are linear, so this equals ROAR's post-render element gain
//!   while keeping one implementation of IAMF parameter timing. The output
//!   mix gain is applied by the decoder after rendering, as for the builtin
//!   backend.
//!
//! ROAR renders fixed-size blocks chosen at creation: the first temporal
//! unit's length, or its largest divisor within ROAR's 16384-sample limit.
//! IAMF temporal units have a constant length; a unit that is not a whole
//! number of blocks has its last block zero-padded (and the padding
//! discarded), like the builtin binaural renderer.

use aom_roar::{
    AudioElementConfig, BinauralFilterProfile, ChannelBasedConfig, Config, DownmixInfo,
    DownmixMode, ElementRenderingConfig, GroupId, HeadphonesRenderingMode, HighOrderAmbisonics,
    Layout, OarError, PlanarBufferMut, PlanarBufferRef, RoarRenderer, SampleRate, Samples,
    SceneBasedConfig,
};

use crate::DecodeError;
use crate::layout::SoundSystem;

/// ROAR's maximum block size (`MAX_SAMPLES_PER_CHANNEL`).
const MAX_BLOCK: usize = 16384;

/// ROAR's maximum number of audio groups.
pub(crate) const MAX_GROUPS: usize = 2;

fn roar_error(context: &str, err: OarError) -> DecodeError {
    DecodeError::UnsupportedRenderer(format!("ROAR {context}: {err}"))
}

/// The ROAR output layout for a target sound system.
pub(crate) fn target_layout(target: SoundSystem) -> Layout {
    match target {
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
    }
}

/// The ROAR input layout for an IAMF `loudspeaker_layout` (0..=8).
fn input_layout(loudspeaker_layout: u8) -> Option<Layout> {
    Some(match loudspeaker_layout {
        0 => Layout::Mono,
        1 => Layout::Stereo,
        2 => Layout::Layout51,
        3 => Layout::Layout512,
        4 => Layout::Layout514,
        5 => Layout::Layout71,
        6 => Layout::Layout712,
        7 => Layout::Layout714,
        8 => Layout::Layout312,
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

fn hoa_order(channels: usize) -> Option<HighOrderAmbisonics> {
    Some(match channels {
        1 => HighOrderAmbisonics::Zoa,
        4 => HighOrderAmbisonics::Order1,
        9 => HighOrderAmbisonics::Order2,
        16 => HighOrderAmbisonics::Order3,
        25 => HighOrderAmbisonics::Order4,
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
        .find(|b| frame_len % b == 0)
        .unwrap_or(MAX_BLOCK)
}

/// What kind of element ROAR is rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ElementKind {
    /// Channel-based: the reconstructed layer's `loudspeaker_layout` and,
    /// for elements with a demixing parameter, the default `dmixp_mode`.
    Channels {
        loudspeaker_layout: u8,
        default_dmixp_mode: Option<u8>,
    },
    /// Scene-based, ACN-ordered planes (order from the plane count).
    Hoa,
}

/// One element's audio for one temporal unit (mix gain already applied).
#[derive(Debug)]
pub(crate) struct ElementInput {
    /// IAMF `audio_element_id` (also the ROAR element id).
    pub(crate) id: u32,
    /// Index of the sub-mix the element belongs to (→ ROAR group).
    pub(crate) group: usize,
    pub(crate) kind: ElementKind,
    /// §3.8.2 headphones_rendering_mode (binaural target only).
    pub(crate) headphones_rendering_mode: u8,
    /// Demixing mode that applies to this unit, if a parameter block
    /// covers it.
    pub(crate) dmixp_mode: Option<u8>,
    /// Planar element channels, one plane per channel, all unit-length.
    pub(crate) planar: Vec<Vec<f32>>,
}

struct ElementState {
    id: u32,
    channels: usize,
    /// Whether ROAR instantiated its downmix renderer for this element
    /// (dynamic demixing-mode updates are only valid then).
    downmix_active: bool,
}

/// A configured ROAR instance for one mix presentation and output layout.
pub(crate) struct RoarMix {
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
            .field("elements", &self.elements.len())
            .finish_non_exhaustive()
    }
}

impl RoarMix {
    /// Creates a ROAR instance for `inputs` (their kinds/ids/groups; the
    /// audio is not consumed) rendering `frame_len`-sample units.
    pub(crate) fn new(
        target: SoundSystem,
        frame_len: usize,
        sample_rate: u32,
        inputs: &[ElementInput],
    ) -> Result<Self, DecodeError> {
        let layout = target_layout(target);
        let block = block_size(frame_len);
        let config = Config::new(
            layout,
            Samples::new(block as u32).map_err(|e| roar_error("block size", e))?,
            SampleRate::new(sample_rate).map_err(|e| roar_error("sample rate", e))?,
        )
        .map_err(|e| roar_error("config", e))?;
        let mut renderer = RoarRenderer::create(&config).map_err(|e| roar_error("create", e))?;

        let groups = inputs.iter().map(|i| i.group + 1).max().unwrap_or(1);
        if groups > MAX_GROUPS {
            return Err(DecodeError::UnsupportedRenderer(format!(
                "ROAR supports at most {MAX_GROUPS} sub-mixes (audio groups), mix has {groups}"
            )));
        }
        let mut group_ids: Vec<GroupId> = Vec::with_capacity(groups);
        for _ in 0..groups {
            group_ids.push(
                renderer
                    .add_audio_group()
                    .map_err(|e| roar_error("add group", e))?,
            );
        }

        let binaural = target == SoundSystem::Binaural;
        let mut elements = Vec::with_capacity(inputs.len());
        for input in inputs {
            let rendering_config = binaural.then(|| ElementRenderingConfig {
                headphones_rendering_mode: match input.headphones_rendering_mode {
                    1 => HeadphonesRenderingMode::WorldLocked,
                    2 => HeadphonesRenderingMode::HeadLocked,
                    _ => HeadphonesRenderingMode::WorldLockedRestricted,
                },
                binaural_filter_profile: BinauralFilterProfile::default(),
            });
            let (element_config, downmix_active) = match input.kind {
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
                ElementKind::Hoa => {
                    let order = hoa_order(input.planar.len()).ok_or_else(|| {
                        DecodeError::UnsupportedRenderer(format!(
                            "{} channels is not a full ambisonics order",
                            input.planar.len()
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
            };
            renderer
                .add_element(group_ids[input.group], input.id, &element_config)
                .map_err(|e| roar_error("add element", e))?;
            elements.push(ElementState {
                id: input.id,
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
    pub(crate) fn render(&mut self, inputs: &[ElementInput]) -> Result<Vec<Vec<f32>>, DecodeError> {
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
        // Zero-padded copies for a trailing partial block (rare: IAMF units
        // have a constant length).
        let mut padded: Vec<Vec<Vec<f32>>> = Vec::new();
        let mut pos = 0usize;
        while pos < unit_len {
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

    fn channels_input(id: u32, layout: u8, planar: Vec<Vec<f32>>) -> ElementInput {
        ElementInput {
            id,
            group: 0,
            kind: ElementKind::Channels {
                loudspeaker_layout: layout,
                default_dmixp_mode: None,
            },
            headphones_rendering_mode: 0,
            dmixp_mode: None,
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
                target_layout(target).channels(),
                target.channels(),
                "{target:?}"
            );
        }
    }

    #[test]
    fn stereo_passthrough() {
        let planar = vec![vec![0.5f32; 64], vec![-0.25f32; 64]];
        let inputs = [channels_input(7, 1, planar.clone())];
        let mut mix = RoarMix::new(SoundSystem::A, 64, 48000, &inputs).unwrap();
        let out = mix.render(&inputs).unwrap();
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
        let inputs = [channels_input(1, 1, planar)];
        let mut mix = RoarMix::new(SoundSystem::A, 64, 48000, &inputs).unwrap();
        let out = mix.render(&inputs).unwrap();
        assert_eq!(out[0].len(), 100);
        assert!(out[0].iter().all(|&s| (s - 1.0).abs() < 1e-6));
    }

    #[test]
    fn too_many_groups_rejected() {
        let mut inputs = vec![
            channels_input(1, 1, vec![vec![0.0; 8]; 2]),
            channels_input(2, 1, vec![vec![0.0; 8]; 2]),
            channels_input(3, 1, vec![vec![0.0; 8]; 2]),
        ];
        for (g, i) in inputs.iter_mut().enumerate() {
            i.group = g;
        }
        assert!(matches!(
            RoarMix::new(SoundSystem::A, 8, 48000, &inputs),
            Err(DecodeError::UnsupportedRenderer(_))
        ));
        inputs[2].group = 1;
        assert!(RoarMix::new(SoundSystem::A, 8, 48000, &inputs).is_ok());
    }

    #[test]
    fn element_set_change_rejected() {
        let inputs = [channels_input(1, 1, vec![vec![0.0; 8]; 2])];
        let mut mix = RoarMix::new(SoundSystem::A, 8, 48000, &inputs).unwrap();
        let other = [channels_input(2, 1, vec![vec![0.0; 8]; 2])];
        assert!(mix.render(&other).is_err());
    }
}
