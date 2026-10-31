//! Native port of the Open Binaural Renderer's rendering pipeline
//! (<https://github.com/google/obr>): loudspeaker/object channels are
//! encoded to ambisonics, mixed into an HOA bed, convolved with SH-domain
//! HRIR filters per ear, and peak-limited.
//!
//! The embedded filter assets (`assets/binaural/`) are extracted from obr
//! and carry its BSD-3-Clause-Clear license and the Open Binaural Renderer
//! Patent License 1.0 — see `assets/binaural/NOTICE`.
//!
//! Engaged for elements with `headphones_rendering_mode == 1`; mode-0
//! elements use the stereo-matrix fallback, matching iamf-tools.

mod convolver;
mod filters;
mod limiter;
mod resampler;
mod sh;
mod speakers;

pub use filters::FilterProfile;
pub use sh::sh_coeffs;

use crate::DecodeError;
use convolver::{FftManager, PartitionedFftFilter, SmallFrameChannel};
use limiter::ObrPeakLimiter;

/// Ambisonic order used for loudspeaker/object content (obr
/// `kDefaultBinauralFiltersAmbisonicOrder`).
const DEFAULT_SPEAKER_ORDER: usize = 3;
/// The embedded filters are 48 kHz recordings.
const FILTER_SAMPLE_RATE: u32 = 48_000;
/// Frames shorter than this use [`SmallFrameChannel`]'s fixed-block
/// convolution; longer ones partition the filters by the frame size, as
/// obr does.
const SMALL_FRAME_BLOCK: usize = 64;
/// Largest frame the renderer accepts (FLAC's largest block size); its
/// memory grows with the frame size.
const MAX_FRAME_SIZE: usize = 65_536;

/// What the renderer consumes for one audio element.
#[derive(Debug, Clone, Copy)]
pub enum BinauralInput {
    /// Channel-based content in rendering order for `loudspeaker_layout`
    /// (0..=8 or expanded parent layout 16..=18).
    Speakers {
        /// Loudspeaker layout ID.
        loudspeaker_layout: u8,
    },
    /// ACN/SN3D ambisonics of the given order (0..=4; order 0 is mono
    /// passthrough to both ears).
    Hoa {
        /// Ambisonics order (0..=4).
        order: usize,
    },
    /// Object-based content (`1..=2` objects) dynamically encoded into 3rd-order
    /// ambisonics per frame.
    Objects {
        /// Number of audio objects (`1` or `2`).
        num_objects: usize,
    },
}

impl core::fmt::Debug for BinauralRenderer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("BinauralRenderer")
            .field("input_channels", &self.input_channels)
            .field("hoa_channels", &self.hoa_channels)
            .field("frame_size", &self.frame_size)
            .finish_non_exhaustive()
    }
}

/// obr-style binaural renderer for one audio element. Stateful across
/// frames (convolution tails, limiter envelope, object SH gain interpolation).
pub struct BinauralRenderer {
    frame_size: usize,
    /// Speaker-to-HOA encoding matrix, `None` for direct HOA or object input.
    encoding_matrix: Option<Vec<Vec<f32>>>,
    input_channels: usize,
    hoa_channels: usize,
    filters_left: Vec<PartitionedFftFilter>,
    filters_right: Vec<PartitionedFftFilter>,
    /// Per-HOA-channel convolution for frames shorter than
    /// [`SMALL_FRAME_BLOCK`] (then `filters_*` are empty).
    small_frames: Vec<SmallFrameChannel>,
    fft: FftManager,
    limiter: ObrPeakLimiter,
    /// Scratch HOA bed, one plane per SH channel.
    bed: Vec<Vec<f32>>,
    /// Previous frame's SH encoding column per object (for linear ramp across
    /// the frame, matching obr `AmbisonicEncoder::ProcessPlanarAudioData`).
    prev_obj_columns: Option<Vec<[f32; 16]>>,
}

impl BinauralRenderer {
    /// Creates a new binaural renderer with [`FilterProfile::Ambient`].
    pub fn new(
        input: BinauralInput,
        frame_size: usize,
        sample_rate: u32,
    ) -> Result<Self, DecodeError> {
        Self::with_profile(input, FilterProfile::Ambient, frame_size, sample_rate)
    }

    /// Creates a new binaural renderer for the specified input type, filter
    /// profile, frame size, and sample rate.
    pub fn with_profile(
        input: BinauralInput,
        profile: FilterProfile,
        frame_size: usize,
        sample_rate: u32,
    ) -> Result<Self, DecodeError> {
        if sample_rate != FILTER_SAMPLE_RATE
            && !resampler::rates_supported(FILTER_SAMPLE_RATE, sample_rate)
        {
            return Err(DecodeError::Unimplemented(
                "unsupported sample rate for binaural rendering",
            ));
        }
        if frame_size == 0 {
            return Err(DecodeError::InvalidDescriptors("empty frames".into()));
        }
        if frame_size > MAX_FRAME_SIZE {
            return Err(DecodeError::Unimplemented(
                "binaural rendering of frames longer than 65536 samples",
            ));
        }

        let (order, encoding_matrix, input_channels) = match input {
            BinauralInput::Speakers { loudspeaker_layout } => {
                let sources = speakers::layout_sources(loudspeaker_layout).ok_or(
                    DecodeError::Unimplemented("unsupported layout for binaural rendering"),
                )?;
                let order = DEFAULT_SPEAKER_ORDER;
                let matrix = sh::encoding_matrix(sources, order);
                (order, Some(matrix), sources.len())
            }
            BinauralInput::Hoa { order: 0 } => {
                return Ok(BinauralRenderer {
                    frame_size,
                    encoding_matrix: None,
                    input_channels: 1,
                    hoa_channels: 0,
                    filters_left: Vec::new(),
                    filters_right: Vec::new(),
                    small_frames: Vec::new(),
                    fft: FftManager::new(SMALL_FRAME_BLOCK),
                    limiter: ObrPeakLimiter::new(sample_rate, 50.0, -0.5),
                    bed: Vec::new(),
                    prev_obj_columns: None,
                });
            }
            BinauralInput::Hoa { order } => {
                if !(1..=4).contains(&order) {
                    return Err(DecodeError::Unimplemented("ambisonics order outside 0..=4"));
                }
                let channels = (order + 1) * (order + 1);
                (order, None, channels)
            }
            BinauralInput::Objects { num_objects } => {
                if !(1..=2).contains(&num_objects) {
                    return Err(DecodeError::InvalidDescriptors(
                        "binaural object count must be 1 or 2".into(),
                    ));
                }
                (DEFAULT_SPEAKER_ORDER, None, num_objects)
            }
        };
        let hoa_channels = (order + 1) * (order + 1);

        let small = frame_size < SMALL_FRAME_BLOCK;
        let mut fft = FftManager::new(if small { SMALL_FRAME_BLOCK } else { frame_size });
        let (mut left, mut right) = filters::sh_hrirs(order, profile)?;
        if sample_rate != FILTER_SAMPLE_RATE {
            // obr resamples the SH-HRIRs to the stream rate at load.
            for plane in left.iter_mut().chain(right.iter_mut()) {
                *plane = resampler::resample(plane, FILTER_SAMPLE_RATE, sample_rate);
            }
        }
        debug_assert_eq!(left.len(), hoa_channels);
        let (filters_left, filters_right, small_frames) = if small {
            let channels = left
                .iter()
                .zip(&right)
                .map(|(l, r)| SmallFrameChannel::new(l, r, SMALL_FRAME_BLOCK, &mut fft))
                .collect();
            (Vec::new(), Vec::new(), channels)
        } else {
            let partitioned = |hrirs: &[Vec<f32>], fft: &mut FftManager| {
                hrirs
                    .iter()
                    .map(|hrir| PartitionedFftFilter::new(hrir, frame_size, fft))
                    .collect()
            };
            (
                partitioned(&left, &mut fft),
                partitioned(&right, &mut fft),
                Vec::new(),
            )
        };

        let prev_obj_columns = if matches!(input, BinauralInput::Objects { .. }) {
            let coeffs = sh_coeffs(0.0, 0.0, DEFAULT_SPEAKER_ORDER);
            let mut col = [0.0f32; 16];
            col.copy_from_slice(&coeffs[..16]);
            Some(vec![col; input_channels])
        } else {
            None
        };

        Ok(BinauralRenderer {
            frame_size,
            encoding_matrix,
            input_channels,
            hoa_channels,
            filters_left,
            filters_right,
            small_frames,
            fft,
            // obr: PeakLimiter(sampling_rate, 50 ms release, -0.5 dB).
            limiter: ObrPeakLimiter::new(sample_rate, 50.0, -0.5),
            bed: Vec::new(),
            prev_obj_columns,
        })
    }

    /// Number of input channels required by this renderer instance.
    pub fn input_channels(&self) -> usize {
        self.input_channels
    }

    /// Renders one frame of planes (length `frame_size` each, padded with
    /// zeros if shorter) to a stereo pair.
    pub fn process(&mut self, planes: &[Vec<f32>]) -> Result<[Vec<f32>; 2], DecodeError> {
        if planes.len() < self.input_channels {
            return Err(DecodeError::InvalidDescriptors(format!(
                "binaural input expects {} channels, got {}",
                self.input_channels,
                planes.len()
            )));
        }
        let frame = self.frame_size;
        if self.hoa_channels == 0 {
            // 0th-order ambisonics (`PassthroughMono` in obr): copy channel 0 to L and R.
            let mut out = [vec![0.0f32; frame], vec![0.0f32; frame]];
            let n = planes[0].len().min(frame);
            out[0][..n].copy_from_slice(&planes[0][..n]);
            out[1][..n].copy_from_slice(&planes[0][..n]);
            self.limiter.process(&mut out);
            return Ok(out);
        }
        // Build the HOA bed.
        self.bed.resize(self.hoa_channels, Vec::new());
        for plane in &mut self.bed {
            plane.clear();
            plane.resize(frame, 0.0);
        }
        match &self.encoding_matrix {
            Some(matrix) => {
                for (o, bed_plane) in self.bed.iter_mut().enumerate() {
                    for (i, input_plane) in planes[..self.input_channels].iter().enumerate() {
                        let gain = matrix[o][i];
                        if gain == 0.0 {
                            continue;
                        }
                        for (b, &s) in bed_plane.iter_mut().zip(input_plane.iter()) {
                            *b += gain * s;
                        }
                    }
                }
            }
            None => {
                for (bed_plane, input_plane) in self.bed.iter_mut().zip(planes) {
                    let n = input_plane.len().min(frame);
                    bed_plane[..n].copy_from_slice(&input_plane[..n]);
                }
            }
        }

        Ok(self.convolve_bed(frame))
    }

    /// Renders one frame of object audio (`1..=2` objects) with dynamic polar
    /// positions to a stereo pair, linearly ramping SH encoding coefficients
    /// across the frame (`obr` `AmbisonicEncoder::ProcessPlanarAudioData`).
    pub fn process_objects(
        &mut self,
        planes: &[Vec<f32>],
        positions: &[crate::objects::PolarCoord],
    ) -> Result<[Vec<f32>; 2], DecodeError> {
        if planes.len() < self.input_channels || positions.len() < self.input_channels {
            return Err(DecodeError::InvalidDescriptors(format!(
                "binaural object input expects {} channels/positions, got {}/{}",
                self.input_channels,
                planes.len(),
                positions.len()
            )));
        }
        let frame = self.frame_size;
        self.bed.resize(self.hoa_channels, Vec::new());
        for plane in &mut self.bed {
            plane.clear();
            plane.resize(frame, 0.0);
        }

        let mut target_cols = Vec::with_capacity(self.input_channels);
        for &pos in &positions[..self.input_channels] {
            let coeffs = sh_coeffs(pos.azimuth, pos.elevation, DEFAULT_SPEAKER_ORDER);
            let mut col = [0.0f32; 16];
            col.copy_from_slice(&coeffs[..16]);
            if pos.distance < 0.1 {
                let norm_dist = (pos.distance / 0.1).clamp(0.0, 1.0);
                for degree in 0..=DEFAULT_SPEAKER_ORDER as i32 {
                    let scale = norm_dist.powi(degree);
                    for order_m in -degree..=degree {
                        let idx = (degree * degree + degree + order_m) as usize;
                        col[idx] *= scale;
                    }
                }
            }
            target_cols.push(col);
        }

        for (ch, input_plane) in planes[..self.input_channels].iter().enumerate() {
            let n = input_plane.len().min(frame);
            let target = &target_cols[ch];
            if let Some(prev_cols) = &self.prev_obj_columns {
                let prev = &prev_cols[ch];
                for (o, bed_plane) in self.bed.iter_mut().enumerate() {
                    let start_gain = prev[o];
                    let end_gain = target[o];
                    if frame <= 1 || start_gain == end_gain {
                        if end_gain != 0.0 {
                            for (b, &s) in bed_plane[..n].iter_mut().zip(&input_plane[..n]) {
                                *b += end_gain * s;
                            }
                        }
                    } else {
                        let step = (end_gain - start_gain) / ((frame - 1) as f32);
                        for (i, (b, &s)) in
                            bed_plane[..n].iter_mut().zip(&input_plane[..n]).enumerate()
                        {
                            *b += (start_gain + (i as f32) * step) * s;
                        }
                    }
                }
            } else {
                for (o, bed_plane) in self.bed.iter_mut().enumerate() {
                    let gain = target[o];
                    if gain != 0.0 {
                        for (b, &s) in bed_plane[..n].iter_mut().zip(&input_plane[..n]) {
                            *b += gain * s;
                        }
                    }
                }
            }
        }
        self.prev_obj_columns = Some(target_cols);

        Ok(self.convolve_bed(frame))
    }

    fn convolve_bed(&mut self, frame: usize) -> [Vec<f32>; 2] {
        let mut out = [vec![0.0f32; frame], vec![0.0f32; frame]];
        let mut spectrum = self.fft.spectrum_scratch();
        if !self.small_frames.is_empty() {
            let [left, right] = &mut out;
            for (channel, bed_plane) in self.small_frames.iter_mut().zip(&self.bed) {
                channel.process_into(bed_plane, left, right, &mut self.fft, &mut spectrum);
            }
            self.limiter.process(&mut out);
            return out;
        }
        for (channel, bed_plane) in self.bed.iter().enumerate() {
            self.fft.forward(bed_plane, &mut spectrum);
            self.filters_left[channel].filter(&spectrum, &mut self.fft);
            self.filters_left[channel].output_into(&mut out[0]);
            self.filters_right[channel].filter(&spectrum, &mut self.fft);
            self.filters_right[channel].output_into(&mut out[1]);
        }

        self.limiter.process(&mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: the convolver used to partition its 8192-tap filters
    /// by the frame size, so one-sample frames cost ~3000x more per
    /// sample than 960-sample ones (a CPU DoS from tiny LPCM frames).
    /// Short frames now use a fixed 64-sample block and must render the
    /// same signal as the frame-partitioned path.
    #[test]
    fn short_frames_render_like_long_frames() {
        let input = BinauralInput::Speakers {
            loudspeaker_layout: 2, // 5.1
        };
        let signal: Vec<Vec<f32>> = (0..6)
            .map(|ch| {
                (0..1024)
                    .map(|n| ((n * (ch + 3)) as f32 * 0.01).sin() * 0.3)
                    .collect()
            })
            .collect();
        let render = |frame: usize| {
            let mut renderer = BinauralRenderer::new(input, frame, 48_000).unwrap();
            let mut out = [Vec::new(), Vec::new()];
            for start in (0..1024).step_by(frame) {
                let planes: Vec<Vec<f32>> = signal
                    .iter()
                    .map(|p| p[start..start + frame].to_vec())
                    .collect();
                let [l, r] = renderer.process(&planes).unwrap();
                out[0].extend(l);
                out[1].extend(r);
            }
            out
        };
        let reference = render(64);
        for frame in [1, 8, 32] {
            let got = render(frame);
            for (g, w) in got.iter().zip(&reference) {
                let max = g
                    .iter()
                    .zip(w)
                    .map(|(a, b)| (a - b).abs())
                    .fold(0.0, f32::max);
                assert!(max < 1e-4, "frame {frame}: max diff {max}");
            }
        }
        assert!(BinauralRenderer::new(input, MAX_FRAME_SIZE + 1, 48_000).is_err());
    }
}
