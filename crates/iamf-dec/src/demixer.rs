//! Scalable channel audio demixer (IAMF §7.2), ported from libiamf v1.1.0
//! demixer.c / fixedp11_5.c.
//!
//! Reconstructs the channels of the target layer's layout from the
//! transmitted (down-mixed) channels: output-gain-up, recursive demix
//! (S1→2, S2→3, S3→5, S5→7, TF2→T2, T2→T4), then recon-gain smoothing.

use crate::channels::{Channel, CHANNEL_COUNT};
use crate::DecodeError;

/// Per-mode down-mix parameters (dmixp_mode 0..=6; 3 and 7 are reserved).
/// (alpha, beta, gamma, delta, w_idx_offset)
const DEMIXING_MATRIX: [(f32, f32, f32, f32, i32); 8] = [
    (1.0, 1.0, 0.707, 0.707, -1),
    (0.707, 0.707, 0.707, 0.707, -1),
    (1.0, 0.866, 0.866, 0.866, -1),
    (0.0, 0.0, 0.0, 0.0, 0),
    (1.0, 1.0, 0.707, 0.707, 1),
    (0.707, 0.707, 0.707, 0.707, 1),
    (1.0, 0.866, 0.866, 0.866, 1),
    (0.0, 0.0, 0.0, 0.0, 0),
];

/// wIdx(k) → w(k) (fixedp11_5.c). 0.4342 is the spec's table value, not
/// an approximation of log10(e).
#[allow(clippy::approx_constant)]
const WIDX2W: [f32; 11] = [
    0.0, 0.0179, 0.0391, 0.0658, 0.1038, 0.25, 0.3962, 0.4342, 0.4609, 0.4821, 0.5,
];

pub struct Demixer {
    frame_size: usize,
    /// Transmitted channels in substream decode order.
    channels_in: Vec<Channel>,
    /// Target layer channels in rendering order.
    channels_out: Vec<Channel>,
    /// (channel, linear gain): output gain to reapply before demixing.
    output_gains: Vec<(Channel, f32)>,
    /// (channel, recon gain): smoothed scale of reconstructed channels.
    recon_gains: Vec<(Channel, f32)>,
    recon_flags: u32,

    mode: usize,
    last_mode: usize,
    w_idx: i32,
    last_w_idx: i32,
    /// Per-channel previous smoothed recon scale factor.
    last_sfavg: [f32; CHANNEL_COUNT],
}

/// Scratch channel planes for one frame, indexed by [`Channel`].
struct ChannelData<'a> {
    planes: [Option<&'a [f32]>; CHANNEL_COUNT],
    /// Owned storage for reconstructed channels.
    computed: [Option<Vec<f32>>; CHANNEL_COUNT],
}

impl<'a> ChannelData<'a> {
    fn get(&self, ch: Channel) -> Option<&[f32]> {
        self.computed[ch.index()]
            .as_deref()
            .or(self.planes[ch.index()])
    }

    fn has(&self, ch: Channel) -> bool {
        self.planes[ch.index()].is_some() || self.computed[ch.index()].is_some()
    }

    fn set(&mut self, ch: Channel, data: Vec<f32>) {
        self.computed[ch.index()] = Some(data);
    }
}

impl Demixer {
    pub fn new(
        frame_size: usize,
        channels_in: Vec<Channel>,
        channels_out: Vec<Channel>,
        output_gains: Vec<(Channel, f32)>,
    ) -> Self {
        Demixer {
            frame_size,
            channels_in,
            channels_out,
            output_gains,
            recon_gains: Vec::new(),
            recon_flags: 0,
            mode: 0,
            last_mode: 0,
            w_idx: 0,
            last_w_idx: 0,
            last_sfavg: [1.0; CHANNEL_COUNT],
        }
    }

    /// Sets demixing info. `w_idx` in 0..=10 selects the default (static)
    /// path: both current and previous state are pinned. Out-of-range
    /// `w_idx` (libiamf passes -1) is the dynamic per-frame path: the mode
    /// rotates and the weight index steps by the mode's offset.
    pub fn set_demixing_info(&mut self, mode: u8, w_idx: i32) -> Result<(), DecodeError> {
        let mode = usize::from(mode);
        if mode == 3 || mode > 6 {
            return Err(DecodeError::InvalidDescriptors(format!(
                "invalid demixing mode {mode}"
            )));
        }
        if !(0..=10).contains(&w_idx) {
            self.last_mode = self.mode;
            self.mode = mode;
            self.last_w_idx = self.w_idx;
            let offset = DEMIXING_MATRIX[mode].4;
            self.w_idx = if offset > 0 {
                (self.last_w_idx + 1).min(10)
            } else {
                (self.last_w_idx - 1).max(0)
            };
        } else {
            if mode != self.mode {
                self.mode = mode;
                self.last_mode = mode;
            }
            if self.w_idx != w_idx {
                self.w_idx = w_idx;
                self.last_w_idx = w_idx;
            }
        }
        Ok(())
    }

    /// Sets recon gains: one linear gain per set flag bit, already mapped
    /// to target-layout channels.
    pub fn set_recon_gains(&mut self, flags: u32, gains: Vec<(Channel, f32)>) {
        self.recon_gains = gains;
        self.recon_flags = flags;
    }

    /// Demixes one frame. `input` are planes matching `channels_in`; the
    /// result matches `channels_out`.
    pub fn demix(&mut self, input: &[Vec<f32>]) -> Result<Vec<Vec<f32>>, DecodeError> {
        if input.len() != self.channels_in.len() {
            return Err(DecodeError::InvalidDescriptors(format!(
                "demixer expects {} channels, got {}",
                self.channels_in.len(),
                input.len()
            )));
        }
        let mut data = ChannelData {
            planes: [None; CHANNEL_COUNT],
            computed: std::array::from_fn(|_| None),
        };
        // Output-gain-up must scale the transmitted planes before any demix
        // math, so gained channels are materialized as computed copies.
        for (ch, plane) in self.channels_in.iter().zip(input) {
            data.planes[ch.index()] = Some(plane);
        }
        for &(ch, gain) in &self.output_gains {
            if let Some(plane) = data.get(ch) {
                data.set(ch, plane.iter().map(|&s| s * gain).collect());
            }
        }

        for c in 0..self.channels_out.len() {
            self.demix_channel(&mut data, self.channels_out[c])?;
        }
        self.apply_recon_gains(&mut data);

        self.channels_out
            .iter()
            .map(|&ch| {
                data.get(ch).map(<[f32]>::to_vec).ok_or_else(|| {
                    DecodeError::InvalidDescriptors(format!("channel {ch:?} missing"))
                })
            })
            .collect()
    }

    fn params(&self) -> (f32, f32, f32, f32) {
        let (a, b, g, d, _) = DEMIXING_MATRIX[self.mode];
        (a, b, g, d)
    }

    fn demix_channel(&self, data: &mut ChannelData, ch: Channel) -> Result<(), DecodeError> {
        if data.has(ch) {
            return Ok(());
        }
        match ch {
            Channel::R2 => self.dmx_s2(data),
            Channel::L3 | Channel::R3 => self.dmx_s3(data),
            Channel::Sl5 | Channel::Sr5 => self.dmx_s5(data),
            Channel::Bl7 | Channel::Br7 => self.dmx_s7(data),
            Channel::Hl | Channel::Hr => self.dmx_h2(data),
            Channel::Hbl | Channel::Hbr => self.dmx_h4(data),
            _ => Err(DecodeError::InvalidDescriptors(format!(
                "channel {ch:?} not transmitted and not reconstructable"
            ))),
        }
    }

    fn need(data: &ChannelData, ch: Channel) -> Result<Vec<f32>, DecodeError> {
        data.get(ch).map(<[f32]>::to_vec).ok_or_else(|| {
            DecodeError::InvalidDescriptors(format!("demix prerequisite {ch:?} missing"))
        })
    }

    /// R2 = 2 x Mono - L2
    fn dmx_s2(&self, data: &mut ChannelData) -> Result<(), DecodeError> {
        if data.has(Channel::R2) {
            return Ok(());
        }
        let mono = Self::need(data, Channel::Mono)?;
        let l2 = Self::need(data, Channel::L2)?;
        data.set(
            Channel::R2,
            mono.iter().zip(&l2).map(|(&m, &l)| 2.0 * m - l).collect(),
        );
        Ok(())
    }

    /// L3 = L2 - 0.707 x C, R3 = R2 - 0.707 x C
    fn dmx_s3(&self, data: &mut ChannelData) -> Result<(), DecodeError> {
        if data.has(Channel::R3) {
            return Ok(());
        }
        self.dmx_s2(data).or_else(|_| {
            // L2/R2 may be transmitted directly without a mono layer.
            if data.has(Channel::R2) {
                Ok(())
            } else {
                Err(DecodeError::InvalidDescriptors(
                    "stereo pair missing".into(),
                ))
            }
        })?;
        let c = Self::need(data, Channel::C)?;
        let l2 = Self::need(data, Channel::L2)?;
        let r2 = Self::need(data, Channel::R2)?;
        data.set(
            Channel::L3,
            l2.iter().zip(&c).map(|(&l, &cc)| l - 0.707 * cc).collect(),
        );
        data.set(
            Channel::R3,
            r2.iter().zip(&c).map(|(&r, &cc)| r - 0.707 * cc).collect(),
        );
        Ok(())
    }

    /// Ls = (L3 - L5) / delta, Rs = (R3 - R5) / delta
    fn dmx_s5(&self, data: &mut ChannelData) -> Result<(), DecodeError> {
        if data.has(Channel::Sr5) {
            return Ok(());
        }
        self.dmx_s3(data)?;
        let (_, _, _, delta) = self.params();
        let l3 = Self::need(data, Channel::L3)?;
        let r3 = Self::need(data, Channel::R3)?;
        let l5 = Self::need(data, Channel::L5)?;
        let r5 = Self::need(data, Channel::R5)?;
        data.set(
            Channel::Sl5,
            l3.iter().zip(&l5).map(|(&a, &b)| (a - b) / delta).collect(),
        );
        data.set(
            Channel::Sr5,
            r3.iter().zip(&r5).map(|(&a, &b)| (a - b) / delta).collect(),
        );
        Ok(())
    }

    /// Lrs = (Ls - alpha x Lss) / beta, Rrs = (Rs - alpha x Rss) / beta
    fn dmx_s7(&self, data: &mut ChannelData) -> Result<(), DecodeError> {
        if data.has(Channel::Br7) {
            return Ok(());
        }
        self.dmx_s5(data)?;
        let (alpha, beta, _, _) = self.params();
        let sl5 = Self::need(data, Channel::Sl5)?;
        let sr5 = Self::need(data, Channel::Sr5)?;
        let sl7 = Self::need(data, Channel::Sl7)?;
        let sr7 = Self::need(data, Channel::Sr7)?;
        data.set(
            Channel::Bl7,
            sl5.iter()
                .zip(&sl7)
                .map(|(&s, &ss)| (s - ss * alpha) / beta)
                .collect(),
        );
        data.set(
            Channel::Br7,
            sr5.iter()
                .zip(&sr7)
                .map(|(&s, &ss)| (s - ss * alpha) / beta)
                .collect(),
        );
        Ok(())
    }

    /// Ltf2 = Ltf3 - w x delta x Ls, Rtf2 = Rtf3 - w x delta x Rs
    fn dmx_h2(&self, data: &mut ChannelData) -> Result<(), DecodeError> {
        if data.has(Channel::Hr) {
            return Ok(());
        }
        self.dmx_s5(data)?;
        let (_, _, _, delta) = self.params();
        let w = WIDX2W[self.w_idx.clamp(0, 10) as usize];
        let tl = Self::need(data, Channel::Tl)?;
        let tr = Self::need(data, Channel::Tr)?;
        let sl5 = Self::need(data, Channel::Sl5)?;
        let sr5 = Self::need(data, Channel::Sr5)?;
        data.set(
            Channel::Hl,
            tl.iter()
                .zip(&sl5)
                .map(|(&t, &s)| t - delta * w * s)
                .collect(),
        );
        data.set(
            Channel::Hr,
            tr.iter()
                .zip(&sr5)
                .map(|(&t, &s)| t - delta * w * s)
                .collect(),
        );
        Ok(())
    }

    /// Ltb = (Ltf2 - Ltf4) / gamma, Rtb = (Rtf2 - Rtf4) / gamma
    fn dmx_h4(&self, data: &mut ChannelData) -> Result<(), DecodeError> {
        if data.has(Channel::Hbr) {
            return Ok(());
        }
        self.dmx_h2(data)?;
        let (_, _, gamma, _) = self.params();
        let hl = Self::need(data, Channel::Hl)?;
        let hr = Self::need(data, Channel::Hr)?;
        let hfl = Self::need(data, Channel::Hfl)?;
        let hfr = Self::need(data, Channel::Hfr)?;
        data.set(
            Channel::Hbl,
            hl.iter()
                .zip(&hfl)
                .map(|(&h, &f)| (h - f) / gamma)
                .collect(),
        );
        data.set(
            Channel::Hbr,
            hr.iter()
                .zip(&hfr)
                .map(|(&h, &f)| (h - f) / gamma)
                .collect(),
        );
        Ok(())
    }

    /// Recon gain smoothing (demixer.c dmx_rms): exponential moving average
    /// over frames (N = 7), applied to reconstructed channels.
    fn apply_recon_gains(&mut self, data: &mut ChannelData) {
        const N: f32 = 7.0;
        for &(ch, sf) in &self.recon_gains {
            let sfavg =
                (2.0 / (N + 1.0)) * sf + (1.0 - 2.0 / (N + 1.0)) * self.last_sfavg[ch.index()];
            if let Some(plane) = data.get(ch) {
                // Steady-state windows (no codec-delay offset): the start
                // window is 1 and the stop window 0, so the crossfade
                // reduces to sfavg.
                data.set(ch, plane.iter().map(|&s| s * sfavg).collect());
            }
            self.last_sfavg[ch.index()] = sfavg;
        }
        let _ = self.frame_size;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::Channel::*;

    /// Stereo layer + 5.1 layer: reconstruct Ls/Rs.
    #[test]
    fn stereo_plus_51_demix() {
        let mut dmx = Demixer::new(
            2,
            vec![L2, R2, L5, R5, C, Lfe],
            vec![L5, R5, C, Lfe, Sl5, Sr5],
            vec![],
        );
        // Mode 0: delta = 0.707.
        dmx.set_demixing_info(0, 0).unwrap();
        // L2 = L5 + 0.707*C + 0.707*Ls (per spec down-mix). Choose values:
        // Ls = 1.0, L5 = 0.5, C = 0.2 => L3 = L5 + delta*Ls = 0.5 + 0.707;
        // L2 = L3 + 0.707*C.
        let ls = 1.0f32;
        let l5 = 0.5f32;
        let c = 0.2f32;
        let l3 = l5 + 0.707 * ls;
        let l2 = l3 + 0.707 * c;
        let input = vec![
            vec![l2; 2],
            vec![0.0; 2], // R2 (don't care)
            vec![l5; 2],
            vec![0.0; 2],
            vec![c; 2],
            vec![0.0; 2],
        ];
        let out = dmx.demix(&input).unwrap();
        // out: [L5, R5, C, LFE, Sl5, Sr5]
        assert!((out[4][0] - ls).abs() < 1e-5, "Ls = {}", out[4][0]);
    }

    /// Recon gain scales reconstructed channels with EMA smoothing.
    #[test]
    fn recon_gain_smoothing() {
        let mut dmx = Demixer::new(1, vec![Mono, L2], vec![L2, R2], vec![]);
        dmx.set_recon_gains(0b101, vec![(R2, 0.5)]);
        let out = dmx.demix(&[vec![1.0], vec![0.6]]).unwrap();
        // R2 raw = 2*1.0 - 0.6 = 1.4; first-frame sfavg =
        // 0.25*0.5 + 0.75*1.0 = 0.875.
        assert!((out[1][0] - 1.4 * 0.875).abs() < 1e-6, "got {}", out[1][0]);
    }

    /// Output gain is applied to transmitted channels before demixing.
    #[test]
    fn output_gain_up() {
        let mut dmx = Demixer::new(1, vec![Mono, L2], vec![L2, R2], vec![(L2, 2.0)]);
        let out = dmx.demix(&[vec![1.0], vec![0.5]]).unwrap();
        assert_eq!(out[0][0], 1.0); // L2 scaled: 0.5*2.0
        assert_eq!(out[1][0], 2.0 * 1.0 - 1.0); // R2 from scaled L2
    }
}
