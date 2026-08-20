//! Channel layouts and sound systems (IAMF §3.7.4 loudspeaker_layout,
//! §7.3.2 sound systems), ported from libiamf v1.1.0 IAMF_layout.c.

use crate::matrices::MatrixLayout;

/// Output sound system (mix presentation layout field, 4 bits).
/// 0..=9 are ITU-R BS.2051 systems A..J; 10..=13 are IAMF extensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundSystem {
    A,
    B,
    C,
    D,
    E,
    F,
    G,
    H,
    I,
    J,
    Ext712,
    Ext312,
    Mono,
    Ext916,
}

impl SoundSystem {
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
            _ => return None,
        })
    }

    pub fn channels(&self) -> usize {
        match self {
            SoundSystem::Mono => 1,
            SoundSystem::A => 2,
            SoundSystem::B => 6,
            SoundSystem::C | SoundSystem::I => 8,
            SoundSystem::D | SoundSystem::Ext712 => 10,
            SoundSystem::E => 11,
            SoundSystem::F | SoundSystem::J => 12,
            SoundSystem::G => 14,
            SoundSystem::Ext312 => 6,
            SoundSystem::Ext916 => 16,
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
        }
    }
}

/// Static info for a channel-based element's loudspeaker_layout (§3.7.4).
pub struct LoudspeakerInfo {
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

/// Loudspeaker layouts 0..=8 (binaural and expanded are not yet supported
/// as inputs).
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
