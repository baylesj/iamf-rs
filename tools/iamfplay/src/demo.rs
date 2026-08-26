//! Synthesizes a demo IAMF stream: a third-order ambisonics scene with a
//! "bee" orbiting the listener's head (two axes of motion), percussive
//! pings at fixed positions, and a soft ambience bed — authored as a
//! standalone LPCM IA sequence with a binaural-capable mix presentation.
//! No external assets; everything is generated.

use iamf_dec::binaural::sh_coeffs;

pub const SAMPLE_RATE: u32 = 48_000;
pub const FRAME: usize = 960;
const ORDER: usize = 3;
const CHANNELS: usize = (ORDER + 1) * (ORDER + 1); // 16
const SECONDS: usize = 24;
const ORBIT_SECONDS: f32 = 6.0;

// ---------------------------------------------------------------------------
// OBU writing
// ---------------------------------------------------------------------------

fn leb128(out: &mut Vec<u8>, mut value: u32) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
}

fn obu(out: &mut Vec<u8>, obu_type: u8, payload: &[u8]) {
    out.push(obu_type << 3);
    leb128(out, payload.len() as u32);
    out.extend_from_slice(payload);
}

fn descriptors() -> Vec<u8> {
    let mut stream = Vec::new();

    // IA sequence header: simple profile.
    let mut p = Vec::new();
    p.extend_from_slice(b"iamf");
    p.push(0); // primary_profile
    p.push(0); // additional_profile
    obu(&mut stream, 31, &p);

    // Codec config: LPCM s16le at 48 kHz.
    let mut p = Vec::new();
    leb128(&mut p, 1); // codec_config_id
    p.extend_from_slice(b"ipcm");
    leb128(&mut p, FRAME as u32);
    p.extend_from_slice(&0i16.to_be_bytes()); // audio_roll_distance
    p.push(0x01); // sample_format_flags: little-endian
    p.push(16); // sample_size
    p.extend_from_slice(&SAMPLE_RATE.to_be_bytes());
    obu(&mut stream, 0, &p);

    // Audio element: scene-based, ambisonics mono mode, identity mapping.
    let mut p = Vec::new();
    leb128(&mut p, 1); // audio_element_id
    p.push(1 << 5); // audio_element_type = scene-based
    leb128(&mut p, 1); // codec_config_id
    leb128(&mut p, CHANNELS as u32); // num_substreams
    for id in 0..CHANNELS as u32 {
        leb128(&mut p, id);
    }
    leb128(&mut p, 0); // num_parameters
    leb128(&mut p, 0); // ambisonics_mode = mono
    p.push(CHANNELS as u8); // output_channel_count
    p.push(CHANNELS as u8); // substream_count
    p.extend(0..CHANNELS as u8); // channel_mapping
    obu(&mut stream, 1, &p);

    // Mix presentation: one sub-mix, headphones_rendering_mode = binaural,
    // authored for stereo and binaural layouts.
    let mut p = Vec::new();
    leb128(&mut p, 1); // mix_presentation_id
    leb128(&mut p, 0); // count_label
    leb128(&mut p, 1); // num_sub_mixes
    leb128(&mut p, 1); // num_audio_elements
    leb128(&mut p, 1); // audio_element_id
    p.push(1 << 6); // headphones_rendering_mode = 1 (binaural)
    leb128(&mut p, 0); // rendering_config_extension_size
    let mix_gain = |p: &mut Vec<u8>, param_id: u32| {
        leb128(p, param_id);
        leb128(p, SAMPLE_RATE); // parameter_rate
        p.push(0x80); // param_definition_mode = 1
        p.extend_from_slice(&0i16.to_be_bytes()); // default_mix_gain (Q7.8)
    };
    mix_gain(&mut p, 100); // element_mix_gain
    mix_gain(&mut p, 101); // output_mix_gain
    leb128(&mut p, 2); // num_layouts
    let loudness = |p: &mut Vec<u8>| {
        p.push(0); // info_type
        p.extend_from_slice(&(-24i16 * 256).to_be_bytes()); // integrated
        p.extend_from_slice(&(-2i16 * 256).to_be_bytes()); // digital_peak
    };
    p.push(2 << 6); // loudspeakers convention, sound_system A (stereo)
    loudness(&mut p);
    p.push(3 << 6); // binaural
    loudness(&mut p);
    obu(&mut stream, 2, &p);

    stream
}

// ---------------------------------------------------------------------------
// Scene synthesis
// ---------------------------------------------------------------------------

/// Deterministic xorshift PRNG (no dependency, reproducible stream).
struct Rng(u64);

impl Rng {
    fn white(&mut self) -> f32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 40) as f32 / (1 << 24) as f32 * 2.0 - 1.0
    }
}

/// Band-limited sawtooth via additive synthesis.
fn saw(phase: f32, harmonics: usize) -> f32 {
    let mut acc = 0.0;
    for k in 1..=harmonics {
        acc += (phase * k as f32 * std::f32::consts::TAU).sin() / k as f32;
    }
    acc * (2.0 / std::f32::consts::PI)
}

/// The bee's direction at time `t`: orbits the head every 6 s while
/// slowly rising overhead and dipping back down over the full scene.
fn bee_direction(t: f32) -> (f32, f32) {
    let azimuth = 360.0 * t / ORBIT_SECONDS;
    let elevation = 40.0 * (std::f32::consts::TAU * t / SECONDS as f32).sin();
    (azimuth, elevation)
}

/// Ping positions cycled every 3 s: front, right, behind, left, overhead.
const PING_DIRECTIONS: [(f32, f32); 5] = [
    (0.0, 0.0),
    (-90.0, 0.0),
    (180.0, 0.0),
    (90.0, 0.0),
    (0.0, 85.0),
];

/// Renders the scene into planar ambisonics channels (SN3D/ACN).
fn synthesize() -> Vec<Vec<f32>> {
    let total = SECONDS * SAMPLE_RATE as usize;
    let mut planes = vec![vec![0.0f32; total]; CHANNELS];
    let mut rng = Rng(0x1a3f_5c71_9e2b_d840);
    let dt = 1.0 / SAMPLE_RATE as f32;

    // Bee: vibrato'd saw with wing-flutter tremolo, direction interpolated
    // per frame to avoid zipper noise.
    let mut phase = 0.0f32;
    for frame_start in (0..total).step_by(FRAME) {
        let t0 = frame_start as f32 * dt;
        let t1 = (frame_start + FRAME) as f32 * dt;
        let (az0, el0) = bee_direction(t0);
        let (az1, el1) = bee_direction(t1);
        let sh0 = sh_coeffs(az0, el0, ORDER);
        let sh1 = sh_coeffs(az1, el1, ORDER);
        for i in 0..FRAME.min(total - frame_start) {
            let t = t0 + i as f32 * dt;
            let f0 = 110.0 + 4.0 * (std::f32::consts::TAU * 5.0 * t).sin();
            phase = (phase + f0 * dt).fract();
            let tremolo = 1.0 - 0.35 * (std::f32::consts::TAU * 27.0 * t).sin().abs();
            let sample = 0.22 * tremolo * saw(phase, 40);
            let lerp = i as f32 / FRAME as f32;
            for (ch, plane) in planes.iter_mut().enumerate() {
                let g = sh0[ch] + (sh1[ch] - sh0[ch]) * lerp;
                plane[frame_start + i] += sample * g;
            }
        }
    }

    // Pings: decaying 1.2 kHz sines at fixed positions every 3 s.
    for (n, &(az, el)) in (0..SECONDS / 3).zip(PING_DIRECTIONS.iter().cycle()) {
        let start = (n * 3 + 1) * SAMPLE_RATE as usize + SAMPLE_RATE as usize / 2;
        let sh = sh_coeffs(az, el, ORDER);
        for i in 0..(SAMPLE_RATE as usize / 3) {
            let t = i as f32 * dt;
            let sample = 0.45 * (std::f32::consts::TAU * 1200.0 * t).sin() * (-t / 0.06).exp();
            for (ch, plane) in planes.iter_mut().enumerate() {
                plane[start + i] += sample * sh[ch];
            }
        }
    }

    // Ambience: soft low-passed noise in W only.
    let mut lp = 0.0f32;
    for s in planes[0].iter_mut() {
        lp += 0.02 * (rng.white() - lp);
        *s += 0.6 * lp;
    }

    planes
}

/// Builds the complete standalone IA sequence (descriptors + frames).
pub fn generate() -> Vec<u8> {
    let planes = synthesize();
    let total = planes[0].len();
    let mut stream = descriptors();

    let mut payload = Vec::with_capacity(FRAME * 2);
    for frame_start in (0..total).step_by(FRAME) {
        for (substream, plane) in planes.iter().enumerate() {
            payload.clear();
            for i in 0..FRAME {
                let x = plane.get(frame_start + i).copied().unwrap_or(0.0);
                let q = (x * 32768.0).round().clamp(-32768.0, 32767.0) as i16;
                payload.extend_from_slice(&q.to_le_bytes());
            }
            obu(&mut stream, 6 + substream as u8, &payload);
        }
    }
    stream
}

#[cfg(test)]
mod tests {
    use super::*;
    use iamf_codecs::DefaultFactory;
    use iamf_dec::layout::SoundSystem;
    use iamf_dec::stream::{StreamDecoder, StreamSettings};

    fn decode(layout: SoundSystem) -> Vec<u8> {
        let stream = generate();
        let split = crate::descriptor_split(&stream).expect("valid demo stream");
        let mut settings = StreamSettings::default();
        settings.layout = layout;
        let mut dec =
            StreamDecoder::new_from_descriptors(&stream[..split], settings, &DefaultFactory)
                .expect("demo descriptors accepted");
        let mut out = Vec::new();
        for chunk in stream[split..].chunks(4096) {
            dec.decode(chunk).expect("demo frames accepted");
            while let Some(unit) = dec.get_output_temporal_unit().expect("render ok") {
                out.extend_from_slice(&unit);
            }
        }
        out
    }

    #[test]
    fn demo_decodes_stereo_and_binaural() {
        for layout in [SoundSystem::A, SoundSystem::Binaural] {
            let pcm = decode(layout);
            assert_eq!(pcm.len(), SECONDS * SAMPLE_RATE as usize * 2 * 2);
            let peak = pcm
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]).unsigned_abs())
                .max()
                .unwrap();
            assert!(peak > 2000, "demo should be audible, peak={peak}");
        }
    }
}
