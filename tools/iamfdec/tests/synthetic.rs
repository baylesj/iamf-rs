//! Hand-built LPCM streams exercising behaviors the fetched conformance
//! vectors don't cover: parameter blocks whose subblocks span several
//! temporal units, temporal-delimiter alignment checking, per-unit trim
//! consistency, and duplicated parameter IDs.

use iamf_codecs::DefaultFactory;
use iamf_dec::DecodeError;
use iamf_dec::layout::SoundSystem;
use iamf_dec::stream::{StreamDecoder, StreamSettings};

const FRAME: usize = 64;

fn leb(value: u32) -> Vec<u8> {
    let mut out = Vec::new();
    let mut v = value;
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            break;
        }
        out.push(byte | 0x80);
    }
    out
}

fn obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![obu_type << 3];
    out.extend(leb(payload.len() as u32));
    out.extend_from_slice(payload);
    out
}

/// Audio frame OBU with trimming fields in the header.
fn frame_obu_trimmed(substream: u8, samples: &[i16], trim_start: u32, trim_end: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    let trimmed = trim_start != 0 || trim_end != 0;
    if trimmed {
        payload.extend(leb(trim_end));
        payload.extend(leb(trim_start));
    }
    for &s in samples {
        payload.extend(s.to_le_bytes());
    }
    let mut out = vec![(6 + substream) << 3 | u8::from(trimmed) << 1];
    out.extend(leb(payload.len() as u32));
    out.extend_from_slice(&payload);
    out
}

fn frame_obu(substream: u8, samples: &[i16]) -> Vec<u8> {
    frame_obu_trimmed(substream, samples, 0, 0)
}

/// Mode-1 demixing parameter block: explicit duration, one subblock per
/// `modes` entry, each covering `subblock_duration` samples.
fn demix_block(parameter_id: u32, subblock_duration: u32, modes: &[u8]) -> Vec<u8> {
    let mut payload = leb(parameter_id);
    payload.extend(leb(subblock_duration * modes.len() as u32));
    payload.extend(leb(subblock_duration));
    for &mode in modes {
        payload.push(mode << 5);
    }
    obu(3, &payload)
}

/// Mode-1 step mix-gain parameter block covering `duration` samples.
fn mix_gain_block(parameter_id: u32, duration: u32, gain_q78: i16) -> Vec<u8> {
    let mut payload = leb(parameter_id);
    payload.extend(leb(duration));
    payload.extend(leb(duration));
    payload.push(0); // animation_type step
    payload.extend(gain_q78.to_be_bytes());
    obu(3, &payload)
}

fn lpcm_codec_config() -> Vec<u8> {
    let mut p = leb(0); // codec_config_id
    p.extend(b"ipcm");
    p.extend(leb(FRAME as u32));
    p.extend(0i16.to_be_bytes()); // audio_roll_distance
    p.push(0x01); // little endian
    p.push(16); // sample_size
    p.extend(48000u32.to_be_bytes());
    obu(0, &p)
}

/// Mode-1 mix-gain parameter definition.
fn mix_gain_param(parameter_id: u32) -> Vec<u8> {
    let mut p = leb(parameter_id);
    p.extend(leb(48000)); // parameter_rate
    p.push(0x80); // mode 1
    p.extend(0i16.to_be_bytes()); // default_mix_gain
    p
}

/// Two-layer scalable element (stereo + 5.1) with a mode-1 demixing
/// parameter, substream ids 0..=3.
fn scalable_element(demix_param_id: u32) -> Vec<u8> {
    let mut p = leb(1); // audio_element_id
    p.push(0x00); // element_type 0 (channel based)
    p.extend(leb(0)); // codec_config_id
    p.extend(leb(4)); // num_substreams
    for id in 0..4u32 {
        p.extend(leb(id));
    }
    p.extend(leb(1)); // num_parameters
    p.extend(leb(1)); // param_definition_type demixing
    p.extend(leb(demix_param_id));
    p.extend(leb(48000)); // parameter_rate
    p.push(0x80); // mode 1
    p.push(0); // default_demixing_mode 0
    p.push(0); // default_weight_index 0
    p.push(2 << 5); // num_layers = 2
    p.push(0x01 << 4); // layer 0: stereo
    p.push(1); // substream_count
    p.push(1); // coupled_substream_count
    p.push(0x02 << 4); // layer 1: 5.1
    p.push(3);
    p.push(1);
    obu(1, &p)
}

/// Single-layer stereo element, substream id 0.
fn stereo_element() -> Vec<u8> {
    let mut p = leb(1);
    p.push(0x00);
    p.extend(leb(0));
    p.extend(leb(1));
    p.extend(leb(0));
    p.extend(leb(0)); // num_parameters
    p.push(1 << 5); // num_layers = 1
    p.push(0x01 << 4); // stereo
    p.push(1);
    p.push(1);
    obu(1, &p)
}

/// Minimal mix presentation (id 42, one element, one 5.1 layout entry).
fn mix_presentation(element_gain_id: u32, output_gain_id: u32) -> Vec<u8> {
    let mut p = leb(42);
    p.extend(leb(0)); // count_label
    p.extend(leb(1)); // num_sub_mixes
    p.extend(leb(1)); // num_audio_elements
    p.extend(leb(1)); // audio_element_id
    p.push(0x00); // headphones_rendering_mode
    p.extend(leb(0)); // rendering_config_extension_size
    p.extend(mix_gain_param(element_gain_id));
    p.extend(mix_gain_param(output_gain_id));
    p.extend(leb(1)); // num_layouts
    p.push(0x80 | (1 << 2)); // ss convention, sound system B (5.1)
    p.push(0x00); // loudness info_type
    p.extend(0i16.to_be_bytes());
    p.extend(0i16.to_be_bytes());
    obu(2, &p)
}

fn descriptors(element: Vec<u8>, mix: Vec<u8>) -> Vec<u8> {
    let mut out = obu(31, b"iamf\x00\x01"); // sequence header, simple/base
    out.extend(lpcm_codec_config());
    out.extend(element);
    out.extend(mix);
    out
}

/// Deterministic non-trivial content for one temporal unit of the
/// scalable element: substreams 0/1 coupled stereo, 2/3 mono.
fn scalable_unit_frames(unit: usize) -> [Vec<i16>; 4] {
    let tone = |ch: usize, k: usize| -> i16 {
        let phase = (unit * FRAME + k) as f32 * 0.07 + ch as f32;
        (phase.sin() * 6000.0) as i16
    };
    let coupled = |base: usize| -> Vec<i16> {
        (0..FRAME)
            .flat_map(|k| [tone(base, k), tone(base + 1, k)])
            .collect()
    };
    let mono = |ch: usize| -> Vec<i16> { (0..FRAME).map(|k| tone(ch, k)).collect() };
    [coupled(0), coupled(2), mono(4), mono(5)]
}

fn decode_all(data: &[u8], layout: SoundSystem) -> Result<Vec<u8>, DecodeError> {
    let settings = StreamSettings {
        layout,
        ..StreamSettings::default()
    };
    let mut decoder = StreamDecoder::new_from_descriptors(data, settings, &DefaultFactory)?;
    decoder.decode(data)?;
    let mut out = Vec::new();
    while decoder.is_temporal_unit_available() {
        out.extend(decoder.get_output_temporal_unit()?.expect("available"));
    }
    Ok(out)
}

fn push_unit(stream: &mut Vec<u8>, unit: usize) {
    for (substream, samples) in scalable_unit_frames(unit).iter().enumerate() {
        stream.extend(frame_obu(substream as u8, samples));
    }
}

/// A demixing block whose two subblocks span two temporal units must
/// decode identically to two per-unit blocks carrying the same modes —
/// and differently from a stream where both units use the first mode.
#[test]
fn spanning_demix_block_matches_per_unit_blocks() {
    let head = descriptors(scalable_element(7), mix_presentation(100, 101));

    let mut spanning = head.clone();
    spanning.extend(demix_block(7, FRAME as u32, &[1, 2]));
    push_unit(&mut spanning, 0);
    push_unit(&mut spanning, 1);

    let mut per_unit = head.clone();
    per_unit.extend(demix_block(7, FRAME as u32, &[1]));
    push_unit(&mut per_unit, 0);
    per_unit.extend(demix_block(7, FRAME as u32, &[2]));
    push_unit(&mut per_unit, 1);

    let mut constant = head.clone();
    constant.extend(demix_block(7, FRAME as u32, &[1, 1]));
    push_unit(&mut constant, 0);
    push_unit(&mut constant, 1);

    let spanning = decode_all(&spanning, SoundSystem::B).unwrap();
    let per_unit = decode_all(&per_unit, SoundSystem::B).unwrap();
    let constant = decode_all(&constant, SoundSystem::B).unwrap();
    assert_eq!(spanning, per_unit, "subblock timeline mismatch");
    assert_ne!(
        spanning, constant,
        "second subblock had no effect (demix mode 2 == mode 1?)"
    );
}

/// Temporal delimiters on unit boundaries are fine; one arriving while a
/// unit is ragged (some substreams missing) is a bitstream error.
#[test]
fn temporal_delimiter_alignment() {
    let head = descriptors(scalable_element(7), mix_presentation(100, 101));

    let mut aligned = head.clone();
    aligned.extend(obu(4, &[]));
    push_unit(&mut aligned, 0);
    aligned.extend(obu(4, &[]));
    push_unit(&mut aligned, 1);
    let decoded = decode_all(&aligned, SoundSystem::A).unwrap();
    assert_eq!(decoded.len(), 2 * FRAME * 2 * 2, "two stereo units");

    let mut ragged = head.clone();
    push_unit(&mut ragged, 0);
    // Unit 1: only substreams 0 and 1 arrive before the delimiter.
    let frames = scalable_unit_frames(1);
    ragged.extend(frame_obu(0, &frames[0]));
    ragged.extend(frame_obu(1, &frames[1]));
    ragged.extend(obu(4, &[]));
    assert!(matches!(
        decode_all(&ragged, SoundSystem::A),
        Err(DecodeError::CorruptPacket(_))
    ));
}

/// §3.9 requires identical trimming in all audio frames of one temporal
/// unit; disagreement is a bitstream error.
#[test]
fn trim_mismatch_within_unit_rejected() {
    let head = descriptors(scalable_element(7), mix_presentation(100, 101));

    let mut consistent = head.clone();
    let frames = scalable_unit_frames(0);
    for (substream, samples) in frames.iter().enumerate() {
        consistent.extend(frame_obu_trimmed(substream as u8, samples, 0, 4));
    }
    let decoded = decode_all(&consistent, SoundSystem::A).unwrap();
    assert_eq!(decoded.len(), (FRAME - 4) * 2 * 2, "end trim applied");

    let mut mismatched = head.clone();
    for (substream, samples) in frames.iter().enumerate() {
        let trim_end = if substream == 2 { 8 } else { 4 };
        mismatched.extend(frame_obu_trimmed(substream as u8, samples, 0, trim_end));
    }
    assert!(matches!(
        decode_all(&mismatched, SoundSystem::A),
        Err(DecodeError::CorruptPacket(_))
    ));
}

/// A parameter id shared by the element and output mix gains (invalid per
/// spec, but must not silently drop a consumer): one -6 dB block applies
/// to both, scaling output by -12 dB.
#[test]
fn duplicate_parameter_id_applies_to_all_consumers() {
    let head = descriptors(stereo_element(), mix_presentation(100, 100));
    let frames: Vec<i16> = (0..FRAME).flat_map(|_| [8192i16, -8192]).collect();

    let mut plain = head.clone();
    plain.extend(frame_obu(0, &frames));

    let mut gained = head.clone();
    gained.extend(mix_gain_block(100, FRAME as u32, -256 * 6)); // -6 dB
    gained.extend(frame_obu(0, &frames));

    let plain = decode_all(&plain, SoundSystem::A).unwrap();
    let gained = decode_all(&gained, SoundSystem::A).unwrap();
    let expected_gain = 10f32.powf(-12.0 / 20.0);
    for (p, g) in plain.chunks_exact(2).zip(gained.chunks_exact(2)) {
        let p = i16::from_le_bytes([p[0], p[1]]);
        let g = i16::from_le_bytes([g[0], g[1]]);
        let expected = (f32::from(p) * expected_gain).round();
        assert!(
            (f32::from(g) - expected).abs() <= 2.0,
            "sample {g} vs expected {expected} (both gain stages must apply)"
        );
    }
}
