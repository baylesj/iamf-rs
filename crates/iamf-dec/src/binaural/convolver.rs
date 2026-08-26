//! Uniformly partitioned overlap-add FFT convolution, following obr's
//! `FftManager` / `PartitionedFftFilter` (which follow Resonance Audio).

use std::sync::Arc;

use realfft::num_complex::Complex;
use realfft::{ComplexToReal, RealFftPlanner, RealToComplex};

/// obr `FftManager::kMinFftSize`.
const MIN_FFT_SIZE: usize = 32;

pub(super) struct FftManager {
    fft_size: usize,
    forward: Arc<dyn RealToComplex<f32>>,
    inverse: Arc<dyn ComplexToReal<f32>>,
    /// Zero-padded time-domain scratch for [`FftManager::forward`].
    input_scratch: Vec<f32>,
}

impl FftManager {
    pub(super) fn new(frame_size: usize) -> Self {
        let fft_size = (2 * frame_size.next_power_of_two()).max(MIN_FFT_SIZE);
        let mut planner = RealFftPlanner::<f32>::new();
        FftManager {
            fft_size,
            forward: planner.plan_fft_forward(fft_size),
            inverse: planner.plan_fft_inverse(fft_size),
            input_scratch: vec![0.0; fft_size],
        }
    }

    pub(super) fn fft_size(&self) -> usize {
        self.fft_size
    }

    pub(super) fn spectrum_scratch(&self) -> Vec<Complex<f32>> {
        vec![Complex::default(); self.fft_size / 2 + 1]
    }

    /// FFT of `time` zero-padded to `fft_size`.
    pub(super) fn forward(&mut self, time: &[f32], spectrum: &mut [Complex<f32>]) {
        let n = time.len().min(self.fft_size);
        self.input_scratch[..n].copy_from_slice(&time[..n]);
        self.input_scratch[n..].fill(0.0);
        self.forward
            .process(&mut self.input_scratch, spectrum)
            .expect("fft sizes match");
    }

    /// Unscaled inverse FFT; caller applies 1/fft_size.
    fn inverse(&self, spectrum: &mut [Complex<f32>], time: &mut [f32]) {
        self.inverse
            .process(spectrum, time)
            .expect("fft sizes match");
    }
}

/// One mono FIR of arbitrary length, convolved in uniform partitions of
/// `frame_size` samples with overlap-add across blocks.
pub(super) struct PartitionedFftFilter {
    kernel_spectra: Vec<Vec<Complex<f32>>>,
    /// Ring buffer of input block spectra.
    input_spectra: Vec<Vec<Complex<f32>>>,
    front: usize,
    /// Previous block's IFFT output (for overlap-add).
    previous: Vec<f32>,
    current: Vec<f32>,
    accumulator: Vec<Complex<f32>>,
}

impl PartitionedFftFilter {
    pub(super) fn new(kernel: &[f32], frame_size: usize, fft: &mut FftManager) -> Self {
        let partitions = kernel.len().div_ceil(frame_size).max(1);
        let kernel_spectra = (0..partitions)
            .map(|p| {
                let start = p * frame_size;
                let end = (start + frame_size).min(kernel.len());
                let mut spectrum = fft.spectrum_scratch();
                fft.forward(&kernel[start..end], &mut spectrum);
                spectrum
            })
            .collect();
        PartitionedFftFilter {
            kernel_spectra,
            input_spectra: vec![vec![Complex::default(); fft.fft_size() / 2 + 1]; partitions],
            front: 0,
            previous: vec![0.0; fft.fft_size()],
            current: vec![0.0; fft.fft_size()],
            accumulator: vec![Complex::default(); fft.fft_size() / 2 + 1],
        }
    }

    /// Feeds one block spectrum and computes this block's filtered time
    /// output (kept internally until [`Self::output_into`]).
    pub(super) fn filter(&mut self, input_spectrum: &[Complex<f32>], fft: &FftManager) {
        let partitions = self.kernel_spectra.len();
        self.input_spectra[self.front].copy_from_slice(input_spectrum);

        for c in &mut self.accumulator {
            *c = Complex::default();
        }
        for i in 0..partitions {
            let idx = (self.front + i) % partitions;
            for ((acc, &inp), &ker) in self
                .accumulator
                .iter_mut()
                .zip(self.input_spectra[idx].iter())
                .zip(self.kernel_spectra[i].iter())
            {
                *acc += inp * ker;
            }
        }
        self.front = (self.front + partitions - 1) % partitions;

        std::mem::swap(&mut self.previous, &mut self.current);
        let mut spectrum = std::mem::take(&mut self.accumulator);
        fft.inverse(&mut spectrum, &mut self.current);
        self.accumulator = spectrum;
        let scale = 1.0 / fft.fft_size() as f32;
        for s in &mut self.current {
            *s *= scale;
        }
    }

    /// Adds this block's filtered output (overlap-add of current and
    /// previous IFFT halves) into `out` (`frame_size` samples).
    pub(super) fn output_into(&self, out: &mut [f32]) {
        let frame = out.len();
        for (i, o) in out.iter_mut().enumerate() {
            *o += self.current[i] + self.previous[i + frame];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unit-impulse kernel must pass the signal through unchanged.
    #[test]
    fn delta_kernel_is_identity() {
        let frame = 64;
        let mut fft = FftManager::new(frame);
        let mut kernel = vec![0.0f32; 256];
        kernel[0] = 1.0;
        let mut filter = PartitionedFftFilter::new(&kernel, frame, &mut fft);

        let input: Vec<f32> = (0..frame).map(|i| (i as f32 * 0.37).sin()).collect();
        let mut spectrum = fft.spectrum_scratch();
        fft.forward(&input, &mut spectrum);
        filter.filter(&spectrum, &fft);
        let mut out = vec![0.0f32; frame];
        filter.output_into(&mut out);
        for (o, i) in out.iter().zip(&input) {
            assert!((o - i).abs() < 1e-4, "{o} vs {i}");
        }
    }

    /// A delayed impulse across a partition boundary must delay the signal.
    #[test]
    fn delayed_kernel_delays() {
        let frame = 32;
        let delay = 40; // crosses into the second partition
        let mut fft = FftManager::new(frame);
        let mut kernel = vec![0.0f32; 96];
        kernel[delay] = 1.0;
        let mut filter = PartitionedFftFilter::new(&kernel, frame, &mut fft);

        // Feed three blocks of a ramp; collect output.
        let signal: Vec<f32> = (0..3 * frame).map(|i| i as f32).collect();
        let mut out_all = Vec::new();
        for block in signal.chunks(frame) {
            let mut spectrum = fft.spectrum_scratch();
            fft.forward(block, &mut spectrum);
            filter.filter(&spectrum, &fft);
            let mut out = vec![0.0f32; frame];
            filter.output_into(&mut out);
            out_all.extend(out);
        }
        for (i, &o) in out_all.iter().enumerate() {
            let expected = if i >= delay { (i - delay) as f32 } else { 0.0 };
            assert!((o - expected).abs() < 1e-2, "sample {i}: {o} vs {expected}");
        }
    }
}
