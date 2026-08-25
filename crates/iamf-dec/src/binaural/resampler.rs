//! Polyphase rational resampler, ported from obr's `Resampler`
//! (windowed-sinc interpolation filter, Hann window, transition bandwidth
//! ratio 13). Used one-shot to resample the 48 kHz SH-HRIR filters to the
//! stream's sample rate, matching obr's `CreateShHrirsFromWav`.

/// obr `kTransitionBandwidthRatio`.
const TRANSITION_BANDWIDTH_RATIO: usize = 13;
/// obr `kMaxSupportedNumFrames`, bounding the interpolation filter.
const MAX_FILTER_LENGTH: usize = 16384;

fn gcd(a: usize, b: usize) -> usize {
    if b == 0 { a } else { gcd(b, a % b) }
}

pub fn rates_supported(source: u32, destination: u32) -> bool {
    let g = gcd(source as usize, destination as usize);
    let max_rate = (source.max(destination) as usize) / g;
    let mut filter_length = max_rate * TRANSITION_BANDWIDTH_RATIO;
    filter_length += filter_length % 2;
    filter_length <= MAX_FILTER_LENGTH
}

/// Resamples one channel from `source` Hz to `destination` Hz (whole
/// buffer, fresh state — obr resets the resampler per HRIR load).
pub fn resample(input: &[f32], source: u32, destination: u32) -> Vec<f32> {
    let g = gcd(source as usize, destination as usize);
    let up_rate = destination as usize / g;
    let down_rate = source as usize / g;
    if up_rate == down_rate {
        return input.to_vec();
    }

    // Windowed-sinc interpolation filter (GenerateInterpolatingFilter).
    let max_rate = up_rate.max(down_rate);
    let sample_rate = source as f32;
    let cutoff_frequency = sample_rate / (2 * max_rate) as f32;
    let mut filter_length = max_rate * TRANSITION_BANDWIDTH_RATIO;
    filter_length += filter_length % 2;

    // Full Hann window (GenerateHannWindow).
    let two_pi = 2.0 * std::f32::consts::PI;
    let scaling = two_pi / (filter_length as f32 - 1.0);
    let mut filter: Vec<f32> = (0..filter_length)
        .map(|i| 0.5 * (1.0 - (scaling * i as f32).cos()))
        .collect();
    let angular_cutoff = two_pi * cutoff_frequency / sample_rate;
    let half = filter_length / 2;
    for (i, f) in filter.iter_mut().enumerate() {
        if i == half {
            *f *= angular_cutoff;
        } else {
            let denominator = i as f32 - filter_length as f32 / 2.0;
            *f *= (angular_cutoff * denominator).sin() / denominator;
        }
    }
    let normalizing = up_rate as f32 / filter.iter().sum::<f32>();
    for f in &mut filter {
        *f *= normalizing;
    }

    // Transpose into flipped polyphase phases (ArrangeFilterAsPolyphase).
    // Deliberate divergence from obr: obr sizes the phases with
    // `transposed_length / max_rate`, which truncates the filter whenever
    // down_rate > up_rate — its 48 kHz->16 kHz HRIR resampling loses ~36 dB
    // (verified against decoder_main; the Resonance Audio lineage divides
    // by up_rate). We use the correct up_rate so every filter tap lands in
    // a phase.
    let transposed_length =
        filter_length + up_rate - (filter_length % up_rate.max(1)).min(filter_length);
    let transposed_length = if filter_length % up_rate == 0 {
        filter_length
    } else {
        transposed_length
    };
    let coeffs_per_phase = transposed_length / up_rate;
    let mut transposed = vec![0.0f32; up_rate * coeffs_per_phase];
    for i in 0..up_rate {
        for j in 0..coeffs_per_phase {
            if j * up_rate + i < filter_length {
                transposed[(coeffs_per_phase - 1 - j) + i * coeffs_per_phase] =
                    filter[j * up_rate + i];
            }
        }
    }

    // Polyphase filtering (Process), zero initial state.
    let input_length = input.len();
    let max_output = input_length * up_rate / down_rate + 1;
    let mut output = vec![0.0f32; max_output];
    let mut time_modulo_up_rate = 0usize;
    let mut input_sample = 0usize;
    let mut output_sample = 0usize;
    while input_sample < input_length {
        let mut filter_index = time_modulo_up_rate * coeffs_per_phase;
        // offset_input_index may be negative: those taps read the zeroed
        // pre-stream state and are skipped.
        let offset_input_index = input_sample as isize - coeffs_per_phase as isize + 1;
        let mut index = offset_input_index.max(0) as usize;
        filter_index += (index as isize - offset_input_index) as usize;
        let mut acc = 0.0f32;
        while index <= input_sample {
            acc += input[index] * transposed[filter_index];
            index += 1;
            filter_index += 1;
        }
        output[output_sample] = acc;
        output_sample += 1;

        time_modulo_up_rate += down_rate;
        input_sample += time_modulo_up_rate / up_rate;
        time_modulo_up_rate %= up_rate;
    }
    // GetNextOutputLength with fresh state.
    let next_len = if (input_length * up_rate) % down_rate == 0 {
        max_output - 1
    } else {
        max_output
    };
    output.truncate(next_len.min(output_sample.max(1)));
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_when_rates_match() {
        let x = vec![1.0, 2.0, 3.0];
        assert_eq!(resample(&x, 48000, 48000), x);
    }

    #[test]
    fn downsample_3_to_1_length() {
        let x = vec![0.5f32; 300];
        let y = resample(&x, 48000, 16000);
        assert_eq!(y.len(), 100);
        // DC should be preserved by the normalized filter (away from the
        // startup transient).
        assert!((y[80] - 0.5).abs() < 1e-3, "{}", y[80]);
    }

    #[test]
    fn rate_support_bounds() {
        assert!(rates_supported(48000, 44100));
        assert!(rates_supported(48000, 16000));
        assert!(rates_supported(48000, 96000));
    }
}
