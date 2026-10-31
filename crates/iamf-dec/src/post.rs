//! Post-processing: loudness normalization and peak limiting, ported from
//! libiamf v1.1.0 (audio_effect_peak_limiter.c, iamf_loudness_process).
//!
//! Both stages are optional. libiamf's decoder enables the limiter by
//! default and loudness normalization only when the caller sets a target
//! loudness; integrators choose per use case.

/// Default peak limiter threshold in dBFS (-1.0 dBFS).
pub const LIMITER_THRESHOLD_DB: f32 = -1.0;
/// Default peak limiter attack duration in seconds (1 ms).
pub const LIMITER_ATTACK_SEC: f32 = 0.001;
/// Default peak limiter release duration in seconds (200 ms).
pub const LIMITER_RELEASE_SEC: f32 = 0.200;
/// Default peak limiter lookahead window in samples (240 samples = 5 ms at 48 kHz).
pub const LIMITER_LOOKAHEAD: usize = 240;

/// f32 sample → s16, matching libiamf's `FLOAT2INT16` (round half to
/// even after clamping).
pub fn quantize_s16(sample: f32) -> i16 {
    (sample * 32768.0)
        .clamp(-32768.0, 32767.0)
        .round_ties_even() as i16
}

/// f32 sample → s32 (f64 intermediate so the scale factor is exact).
pub fn quantize_s32(sample: f32) -> i32 {
    (f64::from(sample) * 2_147_483_648.0)
        .clamp(-2_147_483_648.0, 2_147_483_647.0)
        .round_ties_even() as i32
}

/// Loudness normalization: constant gain of `target_db - content_db`
/// (iamf_loudness_process). `content_db` is the mix presentation's
/// integrated loudness for the rendered layout, Q7.8 → dB.
pub fn normalize_loudness(interleaved: &mut [f32], target_db: f32, content_db: f32) {
    let gain = 10f32.powf((target_db - content_db) / 20.0);
    if gain != 1.0 {
        for s in interleaved {
            *s *= gain;
        }
    }
}

/// Look-ahead peak limiter (audio_effect_peak_limiter.c, USE_TRUEPEAK=0).
///
/// Samples are delayed by `lookahead`; when the delayed peak would exceed
/// the threshold, gain ramps down over the attack time along
/// `1 - (x-1)^2` and recovers over the release time.
#[derive(Debug)]
pub struct PeakLimiter {
    threshold: f32,
    attack_sec: f32,
    release_sec: f32,
    inc_tc: f32,
    channels: usize,
    lookahead: usize,

    current_gain: f32,
    target_start_gain: f32,
    target_end_gain: f32,
    current_tc: f32,
    delay: Vec<Vec<f32>>,
    peaks: Vec<f32>,
    entry_index: usize,
    peak_pos: Option<usize>,
    /// Streaming: output frames still to drop (the delay line's initial
    /// zeros), so released output lines up with the input.
    discard: usize,
}

impl PeakLimiter {
    /// Creates a new peak limiter with the specified threshold in dBFS, sample rate, channel count, and lookahead sample count.
    pub fn new(threshold_db: f32, sample_rate: u32, channels: usize, lookahead: usize) -> Self {
        PeakLimiter {
            threshold: 10f32.powf(threshold_db / 20.0),
            attack_sec: LIMITER_ATTACK_SEC,
            release_sec: LIMITER_RELEASE_SEC,
            inc_tc: 1.0 / sample_rate as f32,
            channels,
            lookahead,
            current_gain: 1.0,
            target_start_gain: -1.0,
            target_end_gain: -1.0,
            current_tc: -1.0,
            delay: vec![vec![0.0; lookahead.max(1)]; channels],
            peaks: vec![0.0; lookahead.max(1)],
            entry_index: 0,
            peak_pos: None,
            discard: lookahead,
        }
    }

    /// `1 - (x-1)^2`, clamped to [0, 1] (curve_accel).
    fn curve_accel(x: f32) -> f32 {
        if x > 1.0 {
            1.0
        } else if x < 0.0 {
            0.0
        } else {
            1.0 - (x - 1.0).powi(2)
        }
    }

    fn compute_target_gain(&mut self, peak: f32) -> f32 {
        if self.current_tc != -1.0 && self.current_tc < self.attack_sec {
            self.current_tc += self.inc_tc;
            let ratio = Self::curve_accel(self.current_tc / self.attack_sec);
            self.current_gain =
                self.target_start_gain - ratio * (self.target_start_gain - self.target_end_gain);
        } else if self.current_tc != -1.0 && self.current_tc < self.release_sec + self.attack_sec {
            self.current_tc += self.inc_tc;
            let ratio = Self::curve_accel((self.current_tc - self.attack_sec) / self.release_sec);
            self.current_gain = self.target_end_gain + ratio * (1.0 - self.target_end_gain);
        } else {
            self.current_gain = 1.0;
        }

        if peak * self.current_gain > self.threshold {
            self.target_start_gain = self.current_gain;
            self.target_end_gain = self.threshold / peak;
            self.current_tc = 0.0;
        }
        self.current_gain
    }

    /// Limits a whole interleaved buffer, compensating the look-ahead delay
    /// (output length equals input length).
    pub fn process(&mut self, interleaved: &[f32]) -> Vec<f32> {
        let mut out = interleaved.to_vec();
        self.process_in_place(&mut out);
        out
    }

    /// In-place variant of [`PeakLimiter::process`]: limits a whole
    /// buffer as one signal, compensating the look-ahead delay. Trailing
    /// samples of a partial frame are left untouched.
    ///
    /// Each call starts and ends the signal, so calling this per block of
    /// a longer signal is wrong: use [`PeakLimiter::push`] and
    /// [`PeakLimiter::finish`] for streaming.
    pub fn process_in_place(&mut self, interleaved: &mut [f32]) {
        let channels = self.channels.max(1);
        let frames = interleaved.len() / channels;
        let mut input = vec![0.0f32; channels];
        let mut output = vec![0.0f32; channels];
        for k in 0..frames + self.lookahead {
            if k < frames {
                input.copy_from_slice(&interleaved[k * channels..(k + 1) * channels]);
            } else {
                input.fill(0.0);
            }
            self.step(&input, &mut output);
            if self.lookahead == 0 {
                interleaved[k * channels..(k + 1) * channels].copy_from_slice(&output);
            } else if k >= self.lookahead {
                // Frame k - lookahead was read lookahead steps ago.
                let at = (k - self.lookahead) * channels;
                interleaved[at..at + channels].copy_from_slice(&output);
            }
        }
    }

    /// Streaming: limits `interleaved` as the next part of one continuous
    /// signal and returns the output it releases. Output lags input by
    /// the look-ahead, so the first calls release fewer samples than they
    /// take; [`PeakLimiter::finish`] releases the rest. The concatenated
    /// output equals [`PeakLimiter::process`] of the whole signal.
    pub fn push(&mut self, interleaved: &[f32]) -> Vec<f32> {
        let channels = self.channels.max(1);
        let mut released = Vec::with_capacity(interleaved.len());
        let mut output = vec![0.0f32; channels];
        for frame in interleaved.chunks_exact(channels) {
            self.step(frame, &mut output);
            self.release(&output, &mut released);
        }
        released
    }

    /// Streaming: ends the signal, releasing the samples still in the
    /// look-ahead delay.
    pub fn finish(&mut self) -> Vec<f32> {
        let channels = self.channels.max(1);
        let silence = vec![0.0f32; channels];
        let mut output = vec![0.0f32; channels];
        let mut released = Vec::new();
        for _ in 0..self.lookahead.saturating_sub(self.discard) {
            self.step(&silence, &mut output);
            self.release(&output, &mut released);
        }
        self.discard = self.lookahead;
        released
    }

    /// Samples a streaming [`PeakLimiter::push`] of `samples` more input
    /// samples would release (all of them once primed).
    pub fn released_by_push(&self, samples: usize) -> usize {
        let channels = self.channels.max(1);
        (samples / channels).saturating_sub(self.discard) * channels
    }

    /// Samples [`PeakLimiter::finish`] would release after `samples` more
    /// input samples were pushed.
    pub fn released_by_finish(&self, samples: usize) -> usize {
        let channels = self.channels.max(1);
        let primed = (samples / channels).min(self.discard);
        (self.lookahead - self.discard + primed) * channels
    }

    fn release(&mut self, frame: &[f32], released: &mut Vec<f32>) {
        if self.discard > 0 {
            self.discard -= 1;
        } else {
            released.extend_from_slice(frame);
        }
    }

    /// One look-ahead step: takes one input frame into the delay line and
    /// writes the gain-applied frame leaving it (the input itself when
    /// there is no look-ahead) to `output`.
    fn step(&mut self, input: &[f32], output: &mut [f32]) {
        let buffer_len = self.peaks.len();
        let idx = self.entry_index;

        #[allow(clippy::single_match_else)] // the None arm is a stateful scan
        let peak = match self.peak_pos {
            Some(pos) => self.peaks[pos],
            None => {
                let mut peak = 0.0f32;
                for i in 0..self.lookahead {
                    let p = self.peaks[(i + idx) % buffer_len];
                    if p > peak {
                        peak = p;
                        self.peak_pos = Some((i + idx) % buffer_len);
                    }
                }
                peak
            }
        };
        let gain = self.compute_target_gain(peak);

        let mut peak_max = 0.0f32;
        for (c, (&x, out)) in input.iter().zip(output.iter_mut()).enumerate() {
            if self.lookahead > 0 {
                *out = self.delay[c][idx] * gain;
                self.delay[c][idx] = x;
            } else {
                *out = x * gain;
            }
            let channel_peak = self.delay[c][idx].abs();
            if channel_peak > peak_max {
                peak_max = channel_peak;
            }
        }

        if self.peak_pos == Some(idx) {
            self.peak_pos = None;
        } else if self.peak_pos.is_none() || self.peaks[self.peak_pos.unwrap()] < peak_max {
            self.peak_pos = Some(idx);
        }
        self.peaks[idx] = peak_max;
        if self.lookahead > 0 {
            self.entry_index = (idx + 1) % buffer_len;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_signal_passes_through() {
        let mut limiter = PeakLimiter::new(LIMITER_THRESHOLD_DB, 48000, 1, LIMITER_LOOKAHEAD);
        let input = vec![0.1f32; 1000];
        let out = limiter.process(&input);
        assert_eq!(out.len(), input.len());
        assert!(out.iter().all(|&s| (s - 0.1).abs() < 1e-6));
    }

    #[test]
    fn loud_signal_is_limited() {
        let mut limiter = PeakLimiter::new(LIMITER_THRESHOLD_DB, 48000, 1, LIMITER_LOOKAHEAD);
        let input = vec![1.5f32; 48000];
        let out = limiter.process(&input);
        let threshold = 10f32.powf(LIMITER_THRESHOLD_DB / 20.0);
        // After the attack settles, output must be at or under threshold.
        let tail = &out[1000..];
        assert!(
            tail.iter().all(|&s| s.abs() <= threshold * 1.001),
            "max {}",
            tail.iter().fold(0f32, |a, &b| a.max(b.abs()))
        );
    }

    /// Regression: the streaming decoder used to call process_in_place
    /// per temporal unit, which flushed the look-ahead each time. That
    /// advanced the attack/release timers 240 extra samples per unit,
    /// reset the delay line, and hid peaks behind unit boundaries: gain
    /// jumps (clicks) and output that depended on the unit size. push()
    /// and finish() continue one signal and must equal limiting it whole.
    #[test]
    fn streaming_equals_whole_buffer() {
        let channels = 2;
        let signal: Vec<f32> = (0..20_000)
            .map(|n| {
                let burst = if (2_000..2_500).contains(&(n / 2)) {
                    1.8
                } else {
                    0.5
                };
                burst * ((n as f32) * 0.013).sin()
            })
            .collect();
        let mut whole = PeakLimiter::new(LIMITER_THRESHOLD_DB, 48000, channels, LIMITER_LOOKAHEAD);
        let reference = whole.process(&signal);
        for unit in [1, 64, 960, 1024] {
            let mut limiter =
                PeakLimiter::new(LIMITER_THRESHOLD_DB, 48000, channels, LIMITER_LOOKAHEAD);
            let mut out = Vec::new();
            let mut pushed = 0;
            for chunk in signal.chunks(unit * channels) {
                let expected = limiter.released_by_push(chunk.len());
                let released = limiter.push(chunk);
                assert_eq!(released.len(), expected);
                out.extend(released);
                pushed += chunk.len();
            }
            assert_eq!(limiter.released_by_finish(0), pushed - out.len());
            out.extend(limiter.finish());
            assert_eq!(out, reference, "unit {unit}");
        }
    }

    #[test]
    fn loudness_normalization_gain() {
        let mut samples = vec![0.5f32; 4];
        normalize_loudness(&mut samples, -24.0, -18.0); // -6 dB
        assert!((samples[0] - 0.5 * 10f32.powf(-6.0 / 20.0)).abs() < 1e-6);
    }
}
