//! Named channels for scalable channel audio (IAMF §7.2), ported from
//! libiamf v1.1.0 (IAMF_types.h, IAMF_layout.c, IAMF_decoder.c).

/// A concrete channel of some loudspeaker layout. `L2`/`L3`/`L5`/`L7` are
/// the front-left channel as mixed for stereo/3.x/5.x/7.x layouts (they
/// carry different content in a scalable stream).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Channel {
    Mono,
    L2,
    R2,
    L3,
    R3,
    // Top pair of 3.1.2.
    Tl,
    Tr,
    L5,
    R5,
    Sl5,
    Sr5,
    // Height pair of x.y.2.
    Hl,
    Hr,
    L7,
    R7,
    Sl7,
    Sr7,
    Bl7,
    Br7,
    // Height quad of x.y.4.
    Hfl,
    Hfr,
    Hbl,
    Hbr,
    C,
    Lfe,
}

pub const CHANNEL_COUNT: usize = 25;

impl Channel {
    pub fn index(self) -> usize {
        self as usize
    }
}

use Channel::*;

/// Rendering-order channels (channel_layout) per loudspeaker_layout 0..=8.
pub fn rendering_channels(loudspeaker_layout: u8) -> Option<&'static [Channel]> {
    Some(match loudspeaker_layout {
        0 => &[Mono],
        1 => &[L2, R2],
        2 => &[L5, R5, C, Lfe, Sl5, Sr5],
        3 => &[L5, R5, C, Lfe, Sl5, Sr5, Hl, Hr],
        4 => &[L5, R5, C, Lfe, Sl5, Sr5, Hfl, Hfr, Hbl, Hbr],
        5 => &[L7, R7, C, Lfe, Sl7, Sr7, Bl7, Br7],
        6 => &[L7, R7, C, Lfe, Sl7, Sr7, Bl7, Br7, Hl, Hr],
        7 => &[L7, R7, C, Lfe, Sl7, Sr7, Bl7, Br7, Hfl, Hfr, Hbl, Hbr],
        8 => &[L3, R3, C, Lfe, Tl, Tr],
        _ => return None,
    })
}

/// Substream-decode-order channels of a layout when it is the first layer
/// (coupled pairs first, then C, then LFE) — `channel_layout[decoding_map]`.
pub fn decoding_channels(loudspeaker_layout: u8) -> Option<&'static [Channel]> {
    Some(match loudspeaker_layout {
        0 => &[Mono],
        1 => &[L2, R2],
        2 => &[L5, R5, Sl5, Sr5, C, Lfe],
        3 => &[L5, R5, Sl5, Sr5, Hl, Hr, C, Lfe],
        4 => &[L5, R5, Sl5, Sr5, Hfl, Hfr, Hbl, Hbr, C, Lfe],
        5 => &[L7, R7, Sl7, Sr7, Bl7, Br7, C, Lfe],
        6 => &[L7, R7, Sl7, Sr7, Bl7, Br7, Hl, Hr, C, Lfe],
        7 => &[L7, R7, Sl7, Sr7, Bl7, Br7, Hfl, Hfr, Hbl, Hbr, C, Lfe],
        8 => &[L3, R3, Tl, Tr, C, Lfe],
        _ => return None,
    })
}

/// (surround, height) counts per loudspeaker_layout.
fn surround_height(loudspeaker_layout: u8) -> (u8, u8) {
    match loudspeaker_layout {
        0 => (1, 0),
        1 => (2, 0),
        2 => (5, 0),
        3 => (5, 2),
        4 => (5, 4),
        5 => (7, 0),
        6 => (7, 2),
        7 => (7, 4),
        8 => (3, 2),
        _ => (0, 0),
    }
}

/// Channels a layer adds on top of the previous layer, in substream decode
/// order (libiamf `iamf_channel_layout_get_new_channels`).
pub fn new_channels(last: Option<u8>, cur: u8) -> Vec<Channel> {
    let Some(last) = last else {
        return decoding_channels(cur)
            .map(|c| c.to_vec())
            .unwrap_or_default();
    };
    let (s1, t1) = surround_height(last);
    let (s2, t2) = surround_height(cur);
    let mut chs = Vec::new();
    if s1 < 5 && 5 <= s2 {
        chs.extend([L5, R5]);
    }
    if s1 < 7 && 7 <= s2 {
        chs.extend([Sl7, Sr7]);
    }
    if t2 != t1 && t2 == 4 {
        chs.extend([Hfl, Hfr]);
    }
    if t2 - t1 == 4 {
        chs.extend([Hbl, Hbr]);
    } else if t1 == 0 && t2 == 2 {
        if s2 < 5 {
            chs.extend([Tl, Tr]);
        } else {
            chs.extend([Hl, Hr]);
        }
    }
    if s1 < 3 && 3 <= s2 {
        chs.extend([C, Lfe]);
    }
    if s1 < 2 && 2 <= s2 {
        chs.push(L2);
    }
    chs
}

/// output_gain_flags bit positions (§3.7.4): bit 5 = L, ..., bit 0 = Rtf.
/// Maps a flag bit to the concrete channel of `layout` it scales
/// (libiamf `iamf_output_gain_channel_map`).
pub fn output_gain_channel(loudspeaker_layout: u8, bit: u8) -> Option<Channel> {
    let (surround, _) = surround_height(loudspeaker_layout);
    match bit {
        5 => match loudspeaker_layout {
            0 => Some(Mono),
            1 => Some(L2),
            8 => Some(L3),
            _ => None,
        },
        4 => match loudspeaker_layout {
            1 => Some(R2),
            8 => Some(R3),
            _ => None,
        },
        3 if surround == 5 => Some(Sl5),
        2 if surround == 5 => Some(Sr5),
        1 => Some(if surround < 5 { Tl } else { Hl }),
        0 => Some(if surround < 5 { Tr } else { Hr }),
        _ => None,
    }
}

/// Recon-gain flag bit positions (§3.10.4): L, C, R, Ls, Rs, Ltf, Rtf,
/// Lb(Lrs), Rb(Rrs), Ltb(Ltr), Rtb(Rtr), LFE.
const RECON_CHANNEL_COUNT: usize = 12;

/// Concrete channel of `layout` for each recon-gain flag bit
/// (libiamf `channel_layout_map`). `None` where the layout has no such
/// channel.
pub fn recon_channel(loudspeaker_layout: u8, bit: u8) -> Option<Channel> {
    const MAP: [[Option<Channel>; RECON_CHANNEL_COUNT]; 9] = [
        // Mono
        [
            Some(Mono),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        ],
        // Stereo
        [
            Some(L2),
            None,
            Some(R2),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        ],
        // 5.1
        [
            Some(L5),
            Some(C),
            Some(R5),
            Some(Sl5),
            Some(Sr5),
            None,
            None,
            None,
            None,
            None,
            None,
            Some(Lfe),
        ],
        // 5.1.2
        [
            Some(L5),
            Some(C),
            Some(R5),
            Some(Sl5),
            Some(Sr5),
            Some(Hl),
            Some(Hr),
            None,
            None,
            None,
            None,
            Some(Lfe),
        ],
        // 5.1.4
        [
            Some(L5),
            Some(C),
            Some(R5),
            Some(Sl5),
            Some(Sr5),
            Some(Hfl),
            Some(Hfr),
            None,
            None,
            Some(Hbl),
            Some(Hbr),
            Some(Lfe),
        ],
        // 7.1
        [
            Some(L7),
            Some(C),
            Some(R7),
            Some(Sl7),
            Some(Sr7),
            None,
            None,
            Some(Bl7),
            Some(Br7),
            None,
            None,
            Some(Lfe),
        ],
        // 7.1.2
        [
            Some(L7),
            Some(C),
            Some(R7),
            Some(Sl7),
            Some(Sr7),
            Some(Hl),
            Some(Hr),
            Some(Bl7),
            Some(Br7),
            None,
            None,
            Some(Lfe),
        ],
        // 7.1.4
        [
            Some(L7),
            Some(C),
            Some(R7),
            Some(Sl7),
            Some(Sr7),
            Some(Hfl),
            Some(Hfr),
            Some(Bl7),
            Some(Br7),
            Some(Hbl),
            Some(Hbr),
            Some(Lfe),
        ],
        // 3.1.2
        [
            Some(L3),
            Some(C),
            Some(R3),
            None,
            None,
            Some(Tl),
            Some(Tr),
            None,
            None,
            None,
            None,
            Some(Lfe),
        ],
    ];
    MAP.get(usize::from(loudspeaker_layout))?[usize::from(bit)]
}

/// `(channel, gain)` pairs for a recon-gain flags word: one entry per set
/// flag bit in ascending order, paired with `gains` in the same order, and
/// dropped (gain consumed) when the target layout lacks the channel
/// (libiamf `iamf_recon_channels_order_update`).
pub fn recon_channel_gains(target_layout: u8, flags: u32, gains: &[f32]) -> Vec<(Channel, f32)> {
    (0..RECON_CHANNEL_COUNT as u8)
        .filter(|bit| flags & (1 << bit) != 0)
        .zip(gains.iter().copied())
        .filter_map(|(bit, gain)| recon_channel(target_layout, bit).map(|ch| (ch, gain)))
        .collect()
}

/// Default recon-gain flags for reconstructing `target` from `first` layer
/// (libiamf `iamf_recon_channels_get_flags`).
pub fn default_recon_flags(first_layout: u8, target_layout: u8) -> u32 {
    if first_layout == target_layout {
        return 0;
    }
    let (s1, t1) = surround_height(first_layout);
    let (s2, t2) = surround_height(target_layout);
    let mut flags = 0u32;
    if s1 != s2 {
        if s2 <= 3 {
            flags |= 1 << 0 | 1 << 2; // L, R
        } else if s2 == 5 {
            flags |= 1 << 3 | 1 << 4; // Ls, Rs
        } else if s2 == 7 {
            flags |= 1 << 7 | 1 << 8; // Lb, Rb
        }
    }
    if t2 != t1 && t2 == 4 {
        flags |= 1 << 9 | 1 << 10; // Ltb, Rtb
    }
    if s2 == 5 && t1 != 0 && t2 == t1 {
        flags |= 1 << 5 | 1 << 6; // Ltf, Rtf
    }
    flags
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `CHANNEL_COUNT` sizes fixed per-channel arrays indexed by
    /// `Channel::index()`; it must track the variant count.
    #[test]
    fn channel_count_matches_last_variant() {
        assert_eq!(Channel::Lfe.index() + 1, CHANNEL_COUNT);
    }

    #[test]
    fn stereo_to_51_new_channels() {
        assert_eq!(new_channels(Some(1), 2), vec![L5, R5, C, Lfe]);
    }

    #[test]
    fn mono_to_stereo_new_channels() {
        assert_eq!(new_channels(Some(0), 1), vec![L2]);
    }

    #[test]
    fn layout_312_to_512_new_channels() {
        assert_eq!(new_channels(Some(8), 3), vec![L5, R5]);
    }

    #[test]
    fn layout_51_to_714_new_channels() {
        assert_eq!(new_channels(Some(2), 7), vec![Sl7, Sr7, Hfl, Hfr, Hbl, Hbr]);
    }

    #[test]
    fn default_recon_51_from_stereo() {
        // Stereo -> 5.1: Ls/Rs are reconstructed.
        let flags = default_recon_flags(1, 2);
        let pairs = recon_channel_gains(2, flags, &[1.0, 1.0]);
        assert_eq!(pairs, vec![(Sl5, 1.0), (Sr5, 1.0)]);
    }

    #[test]
    fn default_recon_512_from_312() {
        // 3.1.2 -> 5.1.2: Ls/Rs (surround 3->5) and Ltf/Rtf (s2==5, heights
        // equal).
        let flags = default_recon_flags(8, 3);
        let pairs = recon_channel_gains(3, flags, &[0.5, 0.5, 0.25, 0.25]);
        assert_eq!(pairs, vec![(Sl5, 0.5), (Sr5, 0.5), (Hl, 0.25), (Hr, 0.25)]);
    }
}
