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
mod sh;
mod speakers;

use crate::DecodeError;
use convolver::{FftManager, PartitionedFftFilter};
use filters::FilterProfile;
use limiter::ObrPeakLimiter;

/// Ambisonic order used for loudspeaker/object content (obr
/// `kDefaultBinauralFiltersAmbisonicOrder`).
const DEFAULT_SPEAKER_ORDER: usize = 3;
/// The embedded filters are 48 kHz recordings.
const FILTER_SAMPLE_RATE: u32 = 48_000;

/// What the renderer consumes for one audio element.
#[derive(Debug, Clone, Copy)]
pub enum BinauralInput {
    /// Channel-based content in rendering order for `loudspeaker_layout`
    /// (0..=8).
    Speakers { loudspeaker_layout: u8 },
    /// ACN/SN3D ambisonics of the given order (1..=4).
    Hoa { order: usize },
}

/// obr-style binaural renderer for one audio element. Stateful across
/// frames (convolution tails, limiter envelope).
pub struct BinauralRenderer {
    frame_size: usize,
    /// Speaker-to-HOA encoding matrix, `None` for direct HOA input.
    encoding_matrix: Option<Vec<Vec<f32>>>,
    input_channels: usize,
    hoa_channels: usize,
    filters_left: Vec<PartitionedFftFilter>,
    filters_right: Vec<PartitionedFftFilter>,
    fft: FftManager,
    limiter: ObrPeakLimiter,
    /// Scratch HOA bed, one plane per SH channel.
    bed: Vec<Vec<f32>>,
}

impl BinauralRenderer {
    pub fn new(
        input: BinauralInput,
        frame_size: usize,
        sample_rate: u32,
    ) -> Result<Self, DecodeError> {
        if sample_rate != FILTER_SAMPLE_RATE {
            return Err(DecodeError::Unimplemented(
                "binaural rendering requires 48 kHz streams (HRIR resampling not implemented)",
            ));
        }
        if frame_size == 0 {
            return Err(DecodeError::InvalidDescriptors("empty frames".into()));
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
            BinauralInput::Hoa { order } => {
                if !(1..=4).contains(&order) {
                    return Err(DecodeError::Unimplemented("ambisonics order outside 1..=4"));
                }
                let channels = (order + 1) * (order + 1);
                (order, None, channels)
            }
        };
        let hoa_channels = (order + 1) * (order + 1);

        let fft = FftManager::new(frame_size);
        let (left, right) = filters::sh_hrirs(order, FilterProfile::Ambient)?;
        debug_assert_eq!(left.len(), hoa_channels);
        let filters_left = left
            .iter()
            .map(|hrir| PartitionedFftFilter::new(hrir, frame_size, &fft))
            .collect();
        let filters_right = right
            .iter()
            .map(|hrir| PartitionedFftFilter::new(hrir, frame_size, &fft))
            .collect();

        Ok(BinauralRenderer {
            frame_size,
            encoding_matrix,
            input_channels,
            hoa_channels,
            filters_left,
            filters_right,
            fft,
            // obr: PeakLimiter(sampling_rate, 50 ms release, -0.5 dB).
            limiter: ObrPeakLimiter::new(sample_rate, 50.0, -0.5),
            bed: Vec::new(),
        })
    }

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

        // SH-domain binaural decode: convolve each bed channel with the
        // per-ear filter for that channel and sum.
        let mut out = [vec![0.0f32; frame], vec![0.0f32; frame]];
        let mut spectrum = self.fft.spectrum_scratch();
        for (channel, bed_plane) in self.bed.iter().enumerate() {
            self.fft.forward(bed_plane, &mut spectrum);
            self.filters_left[channel].filter(&spectrum, &self.fft);
            self.filters_left[channel].output_into(&mut out[0]);
            self.filters_right[channel].filter(&spectrum, &self.fft);
            self.filters_right[channel].output_into(&mut out[1]);
        }

        self.limiter.process(&mut out);
        Ok(out)
    }
}
