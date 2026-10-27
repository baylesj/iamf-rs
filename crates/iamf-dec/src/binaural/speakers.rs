//! Virtual loudspeaker positions per IAMF loudspeaker_layout, ported from
//! obr's `LoudspeakerLayouts` (channel order matches our rendering order).

/// One virtual loudspeaker: azimuth (degrees, positive = left), elevation
/// (degrees, positive = up), distance (meters).
#[derive(Debug, Clone, Copy)]
pub(super) struct Source {
    pub azimuth: f32,
    pub elevation: f32,
    pub distance: f32,
    /// LFE channels are still encoded at their position (obr does not
    /// treat them specially in the encoder); the flag is informational.
    #[allow(dead_code)]
    pub is_lfe: bool,
}

const fn spk(azimuth: f32, elevation: f32) -> Source {
    Source {
        azimuth,
        elevation,
        distance: 1.0,
        is_lfe: false,
    }
}

const fn lfe() -> Source {
    Source {
        azimuth: 0.0,
        elevation: -30.0,
        distance: 1.0,
        is_lfe: true,
    }
}

/// Virtual speakers for loudspeaker_layout 0..=8 and the expanded parent
/// layouts ([`crate::layout::LOUDSPEAKER_LAYOUT_9_1_6`],
/// [`crate::layout::LOUDSPEAKER_LAYOUT_10_2_9_3`],
/// [`crate::layout::LOUDSPEAKER_LAYOUT_7_1_5_4`]), in rendering channel
/// order.
pub(super) fn layout_sources(loudspeaker_layout: u8) -> Option<&'static [Source]> {
    use crate::layout::{
        LOUDSPEAKER_LAYOUT_7_1_5_4, LOUDSPEAKER_LAYOUT_9_1_6, LOUDSPEAKER_LAYOUT_10_2_9_3,
    };

    const C: Source = spk(0.0, 0.0);
    const L30: Source = spk(30.0, 0.0);
    const R30: Source = spk(-30.0, 0.0);
    const L45: Source = spk(45.0, 0.0);
    const R45: Source = spk(-45.0, 0.0);
    const L60: Source = spk(60.0, 0.0);
    const R60: Source = spk(-60.0, 0.0);
    const L90: Source = spk(90.0, 0.0);
    const R90: Source = spk(-90.0, 0.0);
    const L110: Source = spk(110.0, 0.0);
    const R110: Source = spk(-110.0, 0.0);
    const L135: Source = spk(135.0, 0.0);
    const R135: Source = spk(-135.0, 0.0);
    const DEG180: Source = spk(180.0, 0.0);
    const UC: Source = spk(0.0, 45.0);
    const TL30: Source = spk(30.0, 45.0);
    const TR30: Source = spk(-30.0, 45.0);
    const TL45: Source = spk(45.0, 45.0);
    const TR45: Source = spk(-45.0, 45.0);
    const TL90: Source = spk(90.0, 45.0);
    const TR90: Source = spk(-90.0, 45.0);
    const TL135: Source = spk(135.0, 45.0);
    const TR135: Source = spk(-135.0, 45.0);
    const U180: Source = spk(180.0, 45.0);
    const T: Source = spk(0.0, 90.0);
    const BC: Source = spk(0.0, -30.0);
    const BL45: Source = spk(45.0, -30.0);
    const BR45: Source = spk(-45.0, -30.0);
    const BL135: Source = spk(135.0, -30.0);
    const BR135: Source = spk(-135.0, -30.0);
    const LFE: Source = lfe();
    const LFE1: Source = Source {
        azimuth: 45.0,
        elevation: -30.0,
        distance: 1.0,
        is_lfe: true,
    };
    const LFE2: Source = Source {
        azimuth: -45.0,
        elevation: -30.0,
        distance: 1.0,
        is_lfe: true,
    };

    Some(match loudspeaker_layout {
        // Mono
        0 => &[C],
        // Stereo
        1 => &[L30, R30],
        // 5.1
        2 => &[L30, R30, C, LFE, L110, R110],
        // 5.1.2
        3 => &[L30, R30, C, LFE, L110, R110, TL90, TR90],
        // 5.1.4
        4 => &[L30, R30, C, LFE, L110, R110, TL45, TR45, TL135, TR135],
        // 7.1
        5 => &[L30, R30, C, LFE, L90, R90, L135, R135],
        // 7.1.2
        6 => &[L30, R30, C, LFE, L90, R90, L135, R135, TL90, TR90],
        // 7.1.4
        7 => &[
            L30, R30, C, LFE, L90, R90, L135, R135, TL45, TR45, TL135, TR135,
        ],
        // 3.1.2
        8 => &[L45, R45, C, LFE, TL30, TR30],
        // 9.1.6 (expanded parent)
        LOUDSPEAKER_LAYOUT_9_1_6 => &[
            L60, R60, C, LFE1, L135, R135, L30, R30, L90, R90, TL45, TR45, TL135, TR135, TL90, TR90,
        ],
        // 10.2.9.3 / BS.2051-H (expanded parent)
        LOUDSPEAKER_LAYOUT_10_2_9_3 => &[
            L60, R60, C, LFE1, L135, R135, L30, R30, DEG180, LFE2, L90, R90, TL45, TR45, UC, T,
            TL135, TR135, TL90, TR90, U180, BC, BL45, BR45,
        ],
        // 7.1.5.4 (expanded parent)
        LOUDSPEAKER_LAYOUT_7_1_5_4 => &[
            L30, R30, C, LFE, L90, R90, L135, R135, TL45, TR45, T, TL135, TR135, BL45, BR45, BL135,
            BR135,
        ],
        _ => return None,
    })
}
