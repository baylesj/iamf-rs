//! Uniformly partitioned overlap-add FFT convolution, following obr's
//! `FftManager` / `PartitionedFftFilter` (which follow Resonance Audio).

use core::ops::{Add, AddAssign, Mul, Sub};

/// obr `FftManager::kMinFftSize`.
const MIN_FFT_SIZE: usize = 32;

#[derive(Clone, Copy, Default, Debug)]
pub(super) struct Complex {
    pub re: f32,
    pub im: f32,
}

impl Complex {
    #[inline]
    const fn new(re: f32, im: f32) -> Self {
        Self { re, im }
    }

    #[inline]
    const fn conj(self) -> Self {
        Self {
            re: self.re,
            im: -self.im,
        }
    }
}

impl Add for Complex {
    type Output = Self;
    #[inline]
    fn add(self, rhs: Self) -> Self {
        Self::new(self.re + rhs.re, self.im + rhs.im)
    }
}

impl AddAssign for Complex {
    #[inline]
    fn add_assign(&mut self, rhs: Self) {
        self.re += rhs.re;
        self.im += rhs.im;
    }
}

impl Sub for Complex {
    type Output = Self;
    #[inline]
    fn sub(self, rhs: Self) -> Self {
        Self::new(self.re - rhs.re, self.im - rhs.im)
    }
}

impl Mul for Complex {
    type Output = Self;
    #[inline]
    fn mul(self, rhs: Self) -> Self {
        Self::new(
            self.re * rhs.re - self.im * rhs.im,
            self.re * rhs.im + self.im * rhs.re,
        )
    }
}

pub(super) struct FftManager {
    fft_size: usize,
    bitrev: Vec<usize>,
    twiddles: Vec<Complex>,
    /// Transform buffer, reused by every call.
    scratch: Vec<Complex>,
}

impl FftManager {
    pub(super) fn new(frame_size: usize) -> Self {
        let fft_size = (2 * frame_size.next_power_of_two()).max(MIN_FFT_SIZE);
        let bits = fft_size.trailing_zeros();
        let bitrev = (0..fft_size)
            .map(|i| i.reverse_bits() >> (usize::BITS - bits))
            .collect();
        let half = fft_size / 2;
        let twiddles = (0..half)
            .map(|k| {
                let angle = -2.0 * core::f32::consts::PI * (k as f32) / (fft_size as f32);
                Complex::new(angle.cos(), angle.sin())
            })
            .collect();
        FftManager {
            fft_size,
            bitrev,
            twiddles,
            scratch: vec![Complex::default(); fft_size],
        }
    }

    pub(super) fn fft_size(&self) -> usize {
        self.fft_size
    }

    pub(super) fn spectrum_scratch(&self) -> Vec<Complex> {
        vec![Complex::default(); self.fft_size / 2 + 1]
    }

    /// In-place radix-2 transform of `scratch` (conjugated twiddles for
    /// the inverse).
    fn transform_scratch(&mut self, inverse: bool) {
        let n = self.fft_size;
        let buf = &mut self.scratch;
        for (i, &j) in self.bitrev.iter().enumerate() {
            if i < j {
                buf.swap(i, j);
            }
        }
        let mut len = 2;
        while len <= n {
            let half = len / 2;
            let step = n / len;
            for start in (0..n).step_by(len) {
                for k in 0..half {
                    let w = self.twiddles[k * step];
                    let w = if inverse { w.conj() } else { w };
                    let u = buf[start + k];
                    let v = buf[start + k + half] * w;
                    buf[start + k] = u + v;
                    buf[start + k + half] = u - v;
                }
            }
            len <<= 1;
        }
    }

    /// FFT of `time` zero-padded to `fft_size`.
    pub(super) fn forward(&mut self, time: &[f32], spectrum: &mut [Complex]) {
        let n = time.len().min(self.fft_size);
        self.scratch.fill(Complex::default());
        for (dst, &src) in self.scratch[..n].iter_mut().zip(&time[..n]) {
            dst.re = src;
        }
        self.transform_scratch(false);
        let bins = self.fft_size / 2 + 1;
        spectrum[..bins].copy_from_slice(&self.scratch[..bins]);
    }

    /// Unscaled inverse FFT; caller applies 1/fft_size.
    fn inverse(&mut self, spectrum: &[Complex], time: &mut [f32]) {
        let bins = self.fft_size / 2 + 1;
        self.scratch[..bins].copy_from_slice(&spectrum[..bins]);
        let n = self.fft_size;
        for (k, bin) in spectrum.iter().enumerate().take(n / 2).skip(1) {
            self.scratch[n - k] = bin.conj();
        }
        self.transform_scratch(true);
        for (dst, src) in time[..self.fft_size].iter_mut().zip(&self.scratch) {
            *dst = src.re;
        }
    }
}

/// One mono FIR of arbitrary length, convolved in uniform partitions of
/// `frame_size` samples with overlap-add across blocks.
pub(super) struct PartitionedFftFilter {
    kernel_spectra: Vec<Vec<Complex>>,
    /// Ring buffer of input block spectra.
    input_spectra: Vec<Vec<Complex>>,
    front: usize,
    /// Previous block's IFFT output (for overlap-add).
    previous: Vec<f32>,
    current: Vec<f32>,
    accumulator: Vec<Complex>,
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
    pub(super) fn filter(&mut self, input_spectrum: &[Complex], fft: &mut FftManager) {
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
        fft.inverse(&self.accumulator, &mut self.current);
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

/// One HOA channel's convolution with a left and a right kernel for
/// frames shorter than `block` samples, at a bounded cost per sample.
///
/// [`PartitionedFftFilter`] partitions the kernel into frame-sized blocks,
/// so its cost per sample grows as frames shrink (8192 partitions for
/// one-sample frames, ~3000x the cost at 960). This splits each kernel
/// into a `block`-tap head, convolved directly sample by sample, and the
/// rest, convolved block by block by a [`PartitionedFftFilter`] one block
/// behind; the one-block delay is exactly the head's length, so the sum
/// is the full convolution with no added latency.
pub(super) struct SmallFrameChannel {
    block: usize,
    /// Previous block followed by the current (partial) block.
    line: Vec<f32>,
    /// Samples of the current block received so far.
    filled: usize,
    ears: [SmallFrameEar; 2],
}

struct SmallFrameEar {
    head: Vec<f32>,
    tail: Option<PartitionedFftFilter>,
    /// Tail contribution for the current block.
    tail_out: Vec<f32>,
}

impl SmallFrameChannel {
    /// `fft` must be sized for `block`-sample frames.
    pub(super) fn new(left: &[f32], right: &[f32], block: usize, fft: &mut FftManager) -> Self {
        let ear = |kernel: &[f32], fft: &mut FftManager| {
            let split = kernel.len().min(block);
            SmallFrameEar {
                head: kernel[..split].to_vec(),
                tail: (kernel.len() > block)
                    .then(|| PartitionedFftFilter::new(&kernel[block..], block, fft)),
                tail_out: vec![0.0; block],
            }
        };
        SmallFrameChannel {
            block,
            line: vec![0.0; 2 * block],
            filled: 0,
            ears: [ear(left, fft), ear(right, fft)],
        }
    }

    /// Convolves `input` and adds the result into `left` and `right`
    /// (each at least `input.len()` long).
    pub(super) fn process_into(
        &mut self,
        input: &[f32],
        left: &mut [f32],
        right: &mut [f32],
        fft: &mut FftManager,
        spectrum: &mut [Complex],
    ) {
        let block = self.block;
        for (t, &x) in input.iter().enumerate() {
            let j = self.filled;
            self.line[block + j] = x;
            for (ear, out) in self.ears.iter().zip([&mut left[t], &mut right[t]]) {
                let mut acc = ear.tail_out[j];
                // line[block + j - i] is x[n - i]; i < block keeps the
                // index within the previous block.
                for (i, &h) in ear.head.iter().enumerate() {
                    acc += h * self.line[block + j - i];
                }
                *out += acc;
            }
            self.filled += 1;
            if self.filled == block {
                fft.forward(&self.line[block..], spectrum);
                for ear in &mut self.ears {
                    ear.tail_out.fill(0.0);
                    if let Some(tail) = &mut ear.tail {
                        tail.filter(spectrum, fft);
                        tail.output_into(&mut ear.tail_out);
                    }
                }
                self.line.copy_within(block.., 0);
                self.filled = 0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random signal in [-1, 1).
    fn noise(len: usize, seed: u32) -> Vec<f32> {
        let mut state = seed.wrapping_mul(2_654_435_761).max(1);
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as f32 / u32::MAX as f32 * 2.0 - 1.0
            })
            .collect()
    }

    /// Direct convolution, the reference.
    fn convolve(x: &[f32], h: &[f32]) -> Vec<f32> {
        (0..x.len())
            .map(|n| {
                h.iter()
                    .enumerate()
                    .take(n + 1)
                    .map(|(i, &hi)| f64::from(hi) * f64::from(x[n - i]))
                    .sum::<f64>() as f32
            })
            .collect()
    }

    /// Regression for the small-frame cost blow-up: the hybrid path is
    /// the full convolution for any frame size below the block, including
    /// sizes that don't divide it, with no added latency.
    #[test]
    fn small_frame_channel_matches_direct_convolution() {
        const BLOCK: usize = 64;
        let left = noise(1000, 1);
        let right = noise(130, 2); // shorter than two blocks
        let input = noise(3000, 3);
        let want_left = convolve(&input, &left);
        let want_right = convolve(&input, &right);
        for frame in [1, 7, 8, 48, 63] {
            let mut fft = FftManager::new(BLOCK);
            let mut spectrum = fft.spectrum_scratch();
            let mut channel = SmallFrameChannel::new(&left, &right, BLOCK, &mut fft);
            let mut got = [vec![0.0f32; input.len()], vec![0.0f32; input.len()]];
            for start in (0..input.len()).step_by(frame) {
                let end = (start + frame).min(input.len());
                let [l, r] = &mut got;
                channel.process_into(
                    &input[start..end],
                    &mut l[start..end],
                    &mut r[start..end],
                    &mut fft,
                    &mut spectrum,
                );
            }
            for (got, want) in got.iter().zip([&want_left, &want_right]) {
                let max = got
                    .iter()
                    .zip(want)
                    .map(|(a, b)| (a - b).abs())
                    .fold(0.0f32, f32::max);
                assert!(max < 1e-3, "frame {frame}: max diff {max}");
            }
        }
    }

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
        filter.filter(&spectrum, &mut fft);
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
            filter.filter(&spectrum, &mut fft);
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
