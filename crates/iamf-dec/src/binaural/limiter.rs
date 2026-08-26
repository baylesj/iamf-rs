//! obr's peak limiter (`peak_limiter.cc`): instantaneous attack,
//! exponential release, envelope shared across channels.

pub(super) struct ObrPeakLimiter {
    ceiling: f64,
    release_time_constant: f64,
    env: f64,
}

impl ObrPeakLimiter {
    pub(super) fn new(sample_rate: u32, release_ms: f64, ceiling_db: f64) -> Self {
        ObrPeakLimiter {
            ceiling: 10f64.powf(ceiling_db / 20.0),
            release_time_constant: (-3.0 / (f64::from(sample_rate) * release_ms / 1000.0)).exp(),
            env: 1.0,
        }
    }

    fn maximum_required_gain(&self, sample: f64) -> f64 {
        if sample.abs() > self.ceiling {
            self.ceiling / sample.abs()
        } else {
            1.0
        }
    }

    /// Applies the limiter in place to a stereo pair of planes.
    pub(super) fn process(&mut self, channels: &mut [Vec<f32>; 2]) {
        let frames = channels[0].len();
        for frame in 0..frames {
            let max_sample = channels
                .iter()
                .map(|c| f64::from(c[frame].abs()))
                .fold(0.0, f64::max);
            let max_req_gain = self.maximum_required_gain(max_sample);
            if max_req_gain < self.env {
                self.env = max_req_gain;
            } else {
                self.env = self.release_time_constant * (self.env - max_req_gain) + max_req_gain;
            }
            let env = self.env as f32;
            for channel in channels.iter_mut() {
                channel[frame] *= env;
            }
        }
    }
}
