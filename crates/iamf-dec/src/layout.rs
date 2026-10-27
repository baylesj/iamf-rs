//! Channel layouts and sound systems (IAMF §3.7.4 loudspeaker_layout,
//! §7.3.2 sound systems), ported from libiamf v1.1.0 IAMF_layout.c.

use crate::matrices::MatrixLayout;

/// Output target: sound systems 0..=13 (mix presentation layout numbering,
/// also iamf-tools `OutputLayout`) plus binaural (14, iamf-tools
/// `kIAMF_Binaural`).
///
/// With the `binaural` feature, elements whose `headphones_rendering_mode`
/// is 1 go through the native obr-style HRTF renderer (see
/// `crate::binaural`); mode-0 elements — and every element when the
/// feature is off — render through the stereo gain matrices, matching
/// libiamf built without its binauralizer libraries. Binaural-input
/// elements (loudspeaker_layout BINAURAL) pass through unchanged to
/// binaural and stereo output, and cannot be rendered to other layouts
/// (iamf-tools behavior).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SoundSystem {
    /// Sound system A: 2.0 stereo (BS.2051-A: L, R).
    A,
    /// Sound system B: 5.1 surround (BS.2051-B: L, R, C, LFE, Ls, Rs).
    B,
    /// Sound system C: 5.1.2 immersive (BS.2051-C).
    C,
    /// Sound system D: 5.1.4 immersive (BS.2051-D).
    D,
    /// Sound system E: 7.1.2 immersive (BS.2051-E).
    E,
    /// Sound system F: 7.1.4 immersive (BS.2051-F).
    F,
    /// Sound system G: 9.1.4 immersive (BS.2051-G).
    G,
    /// Sound system H: 22.2 immersive (BS.2051-H).
    H,
    /// Sound system I: 7.1 surround (BS.2051-I).
    I,
    /// Sound system J: 7.1.4 immersive (BS.2051-J).
    J,
    /// Extended 7.1.2 layout.
    Ext712,
    /// Extended 3.1.2 layout.
    Ext312,
    /// Single-channel mono.
    Mono,
    /// Extended 9.1.6 layout.
    Ext916,
    /// Binaural headphone rendering (layout 14).
    Binaural,
    /// Extended 7.1.5.4 layout (IAMF v2.0 `sound_system == 14`).
    Ext7154,
}

impl TryFrom<u8> for SoundSystem {
    type Error = ();

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        SoundSystem::from_u8(value).ok_or(())
    }
}

impl SoundSystem {
    /// Maps a raw layout integer ID (0..=14) to a [`SoundSystem`].
    pub fn from_u8(value: u8) -> Option<Self> {
        Some(match value {
            0 => SoundSystem::A,
            1 => SoundSystem::B,
            2 => SoundSystem::C,
            3 => SoundSystem::D,
            4 => SoundSystem::E,
            5 => SoundSystem::F,
            6 => SoundSystem::G,
            7 => SoundSystem::H,
            8 => SoundSystem::I,
            9 => SoundSystem::J,
            10 => SoundSystem::Ext712,
            11 => SoundSystem::Ext312,
            12 => SoundSystem::Mono,
            13 => SoundSystem::Ext916,
            14 => SoundSystem::Binaural,
            _ => return None,
        })
    }

    /// Maps an IAMF bitstream `sound_system` value (§3.7.1, 0..=14) to a
    /// [`SoundSystem`], where 14 is `7.1.5.4` ([`SoundSystem::Ext7154`]).
    pub fn from_bitstream_sound_system(value: u8) -> Option<Self> {
        match value {
            0..=13 => Self::from_u8(value),
            14 => Some(SoundSystem::Ext7154),
            _ => None,
        }
    }

    /// Number of output audio channels for this sound system.
    pub fn channels(&self) -> usize {
        match self {
            SoundSystem::Mono => 1,
            SoundSystem::A | SoundSystem::Binaural => 2,
            SoundSystem::B | SoundSystem::Ext312 => 6,
            SoundSystem::C | SoundSystem::I => 8,
            SoundSystem::D | SoundSystem::Ext712 => 10,
            SoundSystem::E => 11,
            SoundSystem::F | SoundSystem::J => 12,
            SoundSystem::G => 14,
            SoundSystem::Ext916 => 16,
            SoundSystem::Ext7154 => 17,
            SoundSystem::H => 24,
        }
    }

    /// The output-side key into the rendering matrix tables.
    pub fn matrix_layout(&self) -> MatrixLayout {
        match self {
            SoundSystem::A => MatrixLayout::Bs2051A,
            SoundSystem::B => MatrixLayout::Bs2051B,
            SoundSystem::C => MatrixLayout::Bs2051C,
            SoundSystem::D => MatrixLayout::Bs2051D,
            SoundSystem::E => MatrixLayout::Bs2051E,
            SoundSystem::F => MatrixLayout::Bs2051F,
            SoundSystem::G => MatrixLayout::Bs2051G,
            SoundSystem::H => MatrixLayout::Bs2051H,
            SoundSystem::I => MatrixLayout::Bs2051I,
            SoundSystem::J => MatrixLayout::Bs2051J,
            SoundSystem::Ext712 => MatrixLayout::Iamf712,
            SoundSystem::Ext312 => MatrixLayout::Iamf312,
            SoundSystem::Mono => MatrixLayout::Mono,
            SoundSystem::Ext916 => MatrixLayout::Iamf916,
            SoundSystem::Binaural => MatrixLayout::Binaural,
            SoundSystem::Ext7154 => MatrixLayout::Iamf7154,
        }
    }
}

/// Static info for a channel-based element's loudspeaker_layout (§3.7.4).
#[derive(Debug)]
pub struct LoudspeakerInfo {
    /// Number of channels in the layout.
    pub channels: usize,
    /// Position in rendering (channel_layout) order of each channel in
    /// substream-decode order: coupled pairs first, then C, then LFE.
    pub decoding_map: &'static [usize],
    /// Input-side key into the rendering matrix tables.
    pub matrix: MatrixLayout,
}

/// The sound system a loudspeaker_layout corresponds to (used for layer
/// selection: a layer matching the playback sound system needs no
/// rendering conversion).
pub fn loudspeaker_sound_system(loudspeaker_layout: u8) -> Option<SoundSystem> {
    Some(match loudspeaker_layout {
        0 => SoundSystem::Mono,
        1 => SoundSystem::A,
        2 => SoundSystem::B,
        3 => SoundSystem::C,
        4 => SoundSystem::D,
        5 => SoundSystem::I,
        6 => SoundSystem::Ext712,
        7 => SoundSystem::J,
        8 => SoundSystem::Ext312,
        _ => return None,
    })
}

/// §3.7.4 `loudspeaker_layout` of a stereo channel layer.
pub(crate) const LOUDSPEAKER_LAYOUT_STEREO: u8 = 1;

/// §3.7.4 `loudspeaker_layout` of a BINAURAL channel layer: pre-rendered
/// headphone L/R signals.
pub(crate) const LOUDSPEAKER_LAYOUT_BINAURAL: u8 = 9;

/// §3.7.4 `loudspeaker_layout` indicating an `expanded_loudspeaker_layout` follows.
pub const LOUDSPEAKER_LAYOUT_EXPANDED: u8 = 15;

/// Internal speaker-layout ID for the 9.1.6 parent layout of expanded layouts 8..=12.
pub const LOUDSPEAKER_LAYOUT_9_1_6: u8 = 16;

/// Internal speaker-layout ID for the 10.2.9.3 (22.2) parent layout of expanded layouts 13..=15.
pub const LOUDSPEAKER_LAYOUT_10_2_9_3: u8 = 17;

/// Internal speaker-layout ID for the 7.1.5.4 parent layout of expanded layouts 16..=19.
pub const LOUDSPEAKER_LAYOUT_7_1_5_4: u8 = 18;

/// Static info for an expanded channel-based element (`loudspeaker_layout == 15`,
/// `expanded_loudspeaker_layout` 0..=19, IAMF §3.6.2.2).
///
/// Each expanded layout is either a full parent layout (`9.1.6`, `10.2.9.3`,
/// `7.1.5.4`) or a channel subset of a parent layout (`5.1.4`, `7.1.4`,
/// `9.1.6`, `10.2.9.3`, `7.1.5.4`). Decoded substream channels (coupled
/// pairs first, then uncoupled mono channels) are scattered into a
/// zero-filled buffer of `parent_channels` planes at `scatter_map[i]`,
/// producing a full parent-layout buffer ready for the parent's EAR matrix
/// or virtual-loudspeaker binaural renderer.
#[derive(Debug)]
pub struct ExpandedLoudspeakerInfo {
    /// Total channels in the parent layout.
    pub parent_channels: usize,
    /// Expected `substream_count` in the layer descriptor.
    pub substreams: u8,
    /// Expected `coupled_substream_count` in the layer descriptor.
    pub coupled_substreams: u8,
    /// Parent speaker layout ID (`4` = 5.1.4, `7` = 7.1.4,
    /// [`LOUDSPEAKER_LAYOUT_9_1_6`], [`LOUDSPEAKER_LAYOUT_10_2_9_3`], or
    /// [`LOUDSPEAKER_LAYOUT_7_1_5_4`]).
    pub parent_speaker_layout: u8,
    /// Parent layout key into the rendering matrix tables.
    pub parent_matrix: MatrixLayout,
    /// Destination channel index in the parent layout for each decoded
    /// input channel (in decode order: coupled pairs first, then mono).
    pub scatter_map: &'static [usize],
}

/// Expanded loudspeaker layouts 0..=19 (IAMF v1.1 Base-Enhanced 0..=12 and
/// IAMF v2.0 13..=19).
pub fn expanded_loudspeaker_info(expanded: u8) -> Option<&'static ExpandedLoudspeakerInfo> {
    static INFOS: [ExpandedLoudspeakerInfo; 20] = [
        // 0: LFE (in 7.1.4)
        ExpandedLoudspeakerInfo {
            parent_channels: 12,
            substreams: 1,
            coupled_substreams: 0,
            parent_speaker_layout: 7,
            parent_matrix: MatrixLayout::Iamf714,
            scatter_map: &[3],
        },
        // 1: Stereo-S (Ls5, Rs5 in 5.1.4)
        ExpandedLoudspeakerInfo {
            parent_channels: 10,
            substreams: 1,
            coupled_substreams: 1,
            parent_speaker_layout: 4,
            parent_matrix: MatrixLayout::Iamf514,
            scatter_map: &[4, 5],
        },
        // 2: Stereo-SS (Lss7, Rss7 in 7.1.4)
        ExpandedLoudspeakerInfo {
            parent_channels: 12,
            substreams: 1,
            coupled_substreams: 1,
            parent_speaker_layout: 7,
            parent_matrix: MatrixLayout::Iamf714,
            scatter_map: &[4, 5],
        },
        // 3: Stereo-RS (Lrs7, Rrs7 in 7.1.4)
        ExpandedLoudspeakerInfo {
            parent_channels: 12,
            substreams: 1,
            coupled_substreams: 1,
            parent_speaker_layout: 7,
            parent_matrix: MatrixLayout::Iamf714,
            scatter_map: &[6, 7],
        },
        // 4: Stereo-TF (Ltf4, Rtf4 in 7.1.4)
        ExpandedLoudspeakerInfo {
            parent_channels: 12,
            substreams: 1,
            coupled_substreams: 1,
            parent_speaker_layout: 7,
            parent_matrix: MatrixLayout::Iamf714,
            scatter_map: &[8, 9],
        },
        // 5: Stereo-TB (Ltb4, Rtb4 in 7.1.4)
        ExpandedLoudspeakerInfo {
            parent_channels: 12,
            substreams: 1,
            coupled_substreams: 1,
            parent_speaker_layout: 7,
            parent_matrix: MatrixLayout::Iamf714,
            scatter_map: &[10, 11],
        },
        // 6: Top-4ch (Ltf4, Rtf4, Ltb4, Rtb4 in 7.1.4)
        ExpandedLoudspeakerInfo {
            parent_channels: 12,
            substreams: 2,
            coupled_substreams: 2,
            parent_speaker_layout: 7,
            parent_matrix: MatrixLayout::Iamf714,
            scatter_map: &[8, 9, 10, 11],
        },
        // 7: 3.0 (L7, R7, C in 7.1.4)
        ExpandedLoudspeakerInfo {
            parent_channels: 12,
            substreams: 2,
            coupled_substreams: 1,
            parent_speaker_layout: 7,
            parent_matrix: MatrixLayout::Iamf714,
            scatter_map: &[0, 1, 2],
        },
        // 8: 9.1.6 (full 16-ch in 9.1.6: [FL, FR, FC, LFE, BL, BR, FLc, FRc, SiL, SiR, TpFL, TpFR, TpBL, TpBR, TpSiL, TpSiR])
        ExpandedLoudspeakerInfo {
            parent_channels: 16,
            substreams: 9,
            coupled_substreams: 7,
            parent_speaker_layout: LOUDSPEAKER_LAYOUT_9_1_6,
            parent_matrix: MatrixLayout::Iamf916,
            scatter_map: &[6, 7, 0, 1, 8, 9, 4, 5, 10, 11, 14, 15, 12, 13, 2, 3],
        },
        // 9: Stereo-F (FL, FR in 9.1.6)
        ExpandedLoudspeakerInfo {
            parent_channels: 16,
            substreams: 1,
            coupled_substreams: 1,
            parent_speaker_layout: LOUDSPEAKER_LAYOUT_9_1_6,
            parent_matrix: MatrixLayout::Iamf916,
            scatter_map: &[0, 1],
        },
        // 10: Stereo-Si (SiL, SiR in 9.1.6)
        ExpandedLoudspeakerInfo {
            parent_channels: 16,
            substreams: 1,
            coupled_substreams: 1,
            parent_speaker_layout: LOUDSPEAKER_LAYOUT_9_1_6,
            parent_matrix: MatrixLayout::Iamf916,
            scatter_map: &[8, 9],
        },
        // 11: Stereo-TpSi (TpSiL, TpSiR in 9.1.6)
        ExpandedLoudspeakerInfo {
            parent_channels: 16,
            substreams: 1,
            coupled_substreams: 1,
            parent_speaker_layout: LOUDSPEAKER_LAYOUT_9_1_6,
            parent_matrix: MatrixLayout::Iamf916,
            scatter_map: &[14, 15],
        },
        // 12: Top-6ch (TpFL, TpFR, TpSiL, TpSiR, TpBL, TpBR in 9.1.6)
        ExpandedLoudspeakerInfo {
            parent_channels: 16,
            substreams: 3,
            coupled_substreams: 3,
            parent_speaker_layout: LOUDSPEAKER_LAYOUT_9_1_6,
            parent_matrix: MatrixLayout::Iamf916,
            scatter_map: &[10, 11, 14, 15, 12, 13],
        },
        // 13: 10.2.9.3 (full 24-ch in BS.2051-H)
        ExpandedLoudspeakerInfo {
            parent_channels: 24,
            substreams: 16,
            coupled_substreams: 8,
            parent_speaker_layout: LOUDSPEAKER_LAYOUT_10_2_9_3,
            parent_matrix: MatrixLayout::Bs2051H,
            scatter_map: &[
                6, 7, 0, 1, 10, 11, 4, 5, 12, 13, 18, 19, 16, 17, 22, 23, 2, 8, 14, 15, 20, 21, 3,
                9,
            ],
        },
        // 14: LFE-Pair (LFE1, LFE2 in 10.2.9.3)
        ExpandedLoudspeakerInfo {
            parent_channels: 24,
            substreams: 2,
            coupled_substreams: 0,
            parent_speaker_layout: LOUDSPEAKER_LAYOUT_10_2_9_3,
            parent_matrix: MatrixLayout::Bs2051H,
            scatter_map: &[3, 9],
        },
        // 15: Bottom-3ch (BtFL, BtFR, BtFC in 10.2.9.3)
        ExpandedLoudspeakerInfo {
            parent_channels: 24,
            substreams: 2,
            coupled_substreams: 1,
            parent_speaker_layout: LOUDSPEAKER_LAYOUT_10_2_9_3,
            parent_matrix: MatrixLayout::Bs2051H,
            scatter_map: &[22, 23, 21],
        },
        // 16: 7.1.5.4 (full 17-ch in 7.1.5.4: [L, R, C, LFE, Lss, Rss, Lrs, Rrs, Ltf, Rtf, TpC, Ltb, Rtb, BtFL, BtFR, BtBL, BtBR])
        ExpandedLoudspeakerInfo {
            parent_channels: 17,
            substreams: 10,
            coupled_substreams: 7,
            parent_speaker_layout: LOUDSPEAKER_LAYOUT_7_1_5_4,
            parent_matrix: MatrixLayout::Iamf7154,
            scatter_map: &[0, 1, 4, 5, 6, 7, 8, 9, 11, 12, 13, 14, 15, 16, 2, 10, 3],
        },
        // 17: Bottom-4ch (BtFL, BtFR, BtBL, BtBR in 7.1.5.4)
        ExpandedLoudspeakerInfo {
            parent_channels: 17,
            substreams: 2,
            coupled_substreams: 2,
            parent_speaker_layout: LOUDSPEAKER_LAYOUT_7_1_5_4,
            parent_matrix: MatrixLayout::Iamf7154,
            scatter_map: &[13, 14, 15, 16],
        },
        // 18: Top-1ch (TpC in 7.1.5.4)
        ExpandedLoudspeakerInfo {
            parent_channels: 17,
            substreams: 1,
            coupled_substreams: 0,
            parent_speaker_layout: LOUDSPEAKER_LAYOUT_7_1_5_4,
            parent_matrix: MatrixLayout::Iamf7154,
            scatter_map: &[10],
        },
        // 19: Top-5ch (Ltf4, Rtf4, Ltb4, Rtb4, TpC in 7.1.5.4)
        ExpandedLoudspeakerInfo {
            parent_channels: 17,
            substreams: 3,
            coupled_substreams: 2,
            parent_speaker_layout: LOUDSPEAKER_LAYOUT_7_1_5_4,
            parent_matrix: MatrixLayout::Iamf7154,
            scatter_map: &[8, 9, 11, 12, 10],
        },
    ];
    INFOS.get(usize::from(expanded))
}

/// Whether `config` is a scalable channel config carrying binaural input
/// (any layer with loudspeaker_layout BINAURAL; §3.7.4 requires it to be
/// the only layer).
pub(crate) fn is_binaural_input(config: &iamf_obu::descriptors::AudioElementConfig) -> bool {
    matches!(
        config,
        iamf_obu::descriptors::AudioElementConfig::ChannelBased { layers }
            if layers.iter().any(|l| l.loudspeaker_layout == LOUDSPEAKER_LAYOUT_BINAURAL)
    )
}

/// Whether an element with `config` can be rendered to `target`. Binaural
/// input only passes through, to binaural or stereo (sound system A)
/// output, like iamf-tools `AudioElementRendererPassThrough`; iamf-tools
/// has no downmix for it to other loudspeaker layouts, so neither do we.
/// Other elements are not restricted here (expanded layouts, still
/// unimplemented, fail at render time).
pub(crate) fn element_renders_to(
    config: &iamf_obu::descriptors::AudioElementConfig,
    target: SoundSystem,
) -> bool {
    !is_binaural_input(config) || matches!(target, SoundSystem::A | SoundSystem::Binaural)
}

/// Loudspeaker layouts 0..=8 (expanded layouts are not yet supported as
/// inputs; binaural input is reconstructed like stereo and passed through,
/// see [`crate::reconstruct::ChannelReconstructor`]).
pub fn loudspeaker_info(loudspeaker_layout: u8) -> Option<&'static LoudspeakerInfo> {
    static INFOS: [LoudspeakerInfo; 9] = [
        // 0: Mono
        LoudspeakerInfo {
            channels: 1,
            decoding_map: &[0],
            matrix: MatrixLayout::Mono,
        },
        // 1: Stereo
        LoudspeakerInfo {
            channels: 2,
            decoding_map: &[0, 1],
            matrix: MatrixLayout::Stereo,
        },
        // 2: 5.1
        LoudspeakerInfo {
            channels: 6,
            decoding_map: &[0, 1, 4, 5, 2, 3],
            matrix: MatrixLayout::Iamf51,
        },
        // 3: 5.1.2
        LoudspeakerInfo {
            channels: 8,
            decoding_map: &[0, 1, 4, 5, 6, 7, 2, 3],
            matrix: MatrixLayout::Iamf512,
        },
        // 4: 5.1.4
        LoudspeakerInfo {
            channels: 10,
            decoding_map: &[0, 1, 4, 5, 6, 7, 8, 9, 2, 3],
            matrix: MatrixLayout::Iamf514,
        },
        // 5: 7.1
        LoudspeakerInfo {
            channels: 8,
            decoding_map: &[0, 1, 4, 5, 6, 7, 2, 3],
            matrix: MatrixLayout::Iamf71,
        },
        // 6: 7.1.2
        LoudspeakerInfo {
            channels: 10,
            decoding_map: &[0, 1, 4, 5, 6, 7, 8, 9, 2, 3],
            matrix: MatrixLayout::Iamf712,
        },
        // 7: 7.1.4
        LoudspeakerInfo {
            channels: 12,
            decoding_map: &[0, 1, 4, 5, 6, 7, 8, 9, 10, 11, 2, 3],
            matrix: MatrixLayout::Iamf714,
        },
        // 8: 3.1.2
        LoudspeakerInfo {
            channels: 6,
            decoding_map: &[0, 1, 4, 5, 2, 3],
            matrix: MatrixLayout::Iamf312,
        },
    ];
    INFOS.get(usize::from(loudspeaker_layout))
}
