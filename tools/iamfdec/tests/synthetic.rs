//! Hand-built LPCM streams exercising behaviors the fetched conformance
//! vectors don't cover: parameter blocks whose subblocks span several
//! temporal units, temporal-delimiter alignment checking, per-unit trim
//! consistency, duplicated parameter IDs, redundant sequence headers that
//! disagree with the canonical one, mix presentations whose sub mix has
//! no audio elements, §7.4.1 mix selection among several mix
//! presentations, binaural-input element passthrough, the default
//! (IAMF v1.1) requested profiles, and the external renderer hook
//! (`iamf_dec::renderer`).

use std::sync::{Arc, Mutex};

use iamf_codecs::DefaultFactory;
use iamf_dec::DecodeError;
use iamf_dec::layout::SoundSystem;
use iamf_dec::presentation::{Descriptors, PresentationDecoder};
use iamf_dec::profile::ProfileSet;
use iamf_dec::renderer::{
    ElementFrame, ElementInfo, ElementLayout, MixConfig, MixRenderer, MixRendererFactory,
};
use iamf_dec::stream::{MixSelection, StreamDecoder, StreamSettings};
use iamf_obu::ObuIter;

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
    let mut settings = StreamSettings::default();
    settings.layout = Some(layout);
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

/// A redundant IA sequence header injected ahead of the canonical one
/// (test_000079 shape) must not override it: here the copy disagrees and
/// declares base-enhanced only, while the canonical header declares
/// simple. A simple-only decoder must follow the canonical header.
#[test]
fn canonical_sequence_header_overrides_redundant_copy() {
    let redundant_copy = {
        let mut out = vec![31 << 3 | 1 << 2]; // obu_redundant_copy = 1
        out.extend(leb(6));
        out.extend(b"iamf\x02\x02");
        out
    };
    let mut canonical = obu(31, b"iamf\x00\x00");
    canonical.extend(lpcm_codec_config());
    canonical.extend(stereo_element());
    canonical.extend(mix_presentation(100, 101));
    let frames: Vec<i16> = (0..FRAME).flat_map(|k| [k as i16, -(k as i16)]).collect();
    canonical.extend(frame_obu(0, &frames));
    let mut injected = redundant_copy;
    injected.extend_from_slice(&canonical);

    let decode_simple = |data: &[u8]| -> Result<Vec<u8>, DecodeError> {
        let mut settings = StreamSettings::default();
        settings.requested_profiles = ProfileSet::SIMPLE;
        let mut decoder = StreamDecoder::new_from_descriptors(data, settings, &DefaultFactory)?;
        decoder.decode(data)?;
        let mut out = Vec::new();
        while let Some(unit) = decoder.get_output_temporal_unit()? {
            out.extend(unit);
        }
        Ok(out)
    };
    let expected = decode_simple(&canonical).unwrap();
    assert_eq!(expected.len(), FRAME * 2 * 2);
    assert_eq!(decode_simple(&injected).unwrap(), expected);
}

/// Mix presentation `id` whose single sub mix holds `elements` — possibly
/// none, which §3.7 forbids (num_audio_elements SHALL NOT be 0) — with one
/// stereo layout entry.
fn mix_presentation_of(id: u32, elements: &[u32]) -> Vec<u8> {
    let mut p = leb(id);
    p.extend(leb(0)); // count_label
    p.extend(leb(1)); // num_sub_mixes
    p.extend(leb(elements.len() as u32)); // num_audio_elements
    for &element in elements {
        p.extend(leb(element));
        p.push(0x00); // headphones_rendering_mode
        p.extend(leb(0)); // rendering_config_extension_size
        p.extend(mix_gain_param(100 + element));
    }
    p.extend(mix_gain_param(200 + id));
    p.extend(leb(1)); // num_layouts
    p.push(0x80); // ss convention, sound system A (stereo)
    p.push(0x00); // loudness info_type
    p.extend(0i16.to_be_bytes());
    p.extend(0i16.to_be_bytes());
    obu(2, &p)
}

/// Single-layer channel element with its own substream (id ==
/// audio_element_id) in `loudspeaker_layout`, coupled.
fn single_layer_element(audio_element_id: u32, loudspeaker_layout: u8) -> Vec<u8> {
    let mut p = leb(audio_element_id);
    p.push(0x00); // element_type 0 (channel based)
    p.extend(leb(0)); // codec_config_id
    p.extend(leb(1)); // num_substreams
    p.extend(leb(audio_element_id)); // substream id
    p.extend(leb(0)); // num_parameters
    p.push(1 << 5); // num_layers = 1
    p.push(loudspeaker_layout << 4);
    p.push(1); // substream_count
    p.push(1); // coupled_substream_count
    obu(1, &p)
}

/// One-element mix presentation declaring a single loudness layout
/// (`layout_byte`: layout_type << 6 | sound_system << 2).
fn single_element_mix(
    mix_id: u32,
    element_id: u32,
    rendering_mode: u8,
    layout_byte: u8,
) -> Vec<u8> {
    let mut p = leb(mix_id);
    p.extend(leb(0)); // count_label
    p.extend(leb(1)); // num_sub_mixes
    p.extend(leb(1)); // num_audio_elements
    p.extend(leb(element_id));
    p.push(rendering_mode << 6); // headphones_rendering_mode
    p.extend(leb(0)); // rendering_config_extension_size
    p.extend(mix_gain_param(100));
    p.extend(mix_gain_param(101));
    p.extend(leb(1)); // num_layouts
    p.push(layout_byte);
    p.push(0x00); // loudness info_type
    p.extend(0i16.to_be_bytes());
    p.extend(0i16.to_be_bytes());
    obu(2, &p)
}

/// One stereo temporal unit after descriptors holding an empty mix
/// (id 7) and, when `with_valid_mix`, a renderable stereo mix (id 8).
fn empty_sub_mix_stream(with_valid_mix: bool) -> Vec<u8> {
    let mut mixes = mix_presentation_of(7, &[]);
    if with_valid_mix {
        mixes.extend(mix_presentation_of(8, &[1]));
    }
    let mut data = descriptors(stereo_element(), mixes);
    let samples: Vec<i16> = (0..FRAME).flat_map(|k| [k as i16, -(k as i16)]).collect();
    data.extend(frame_obu(0, &samples));
    data
}

/// Batch driver: the descriptors still parse (one bad mix must not sink
/// the others), but rendering the empty sub mix is refused instead of
/// yielding zero channels at rate 0 (iamf-tools ac2fff70b).
#[test]
fn empty_sub_mix_batch_rejected() {
    let data = empty_sub_mix_stream(true);
    let descriptors = Descriptors::collect(&data).unwrap();
    assert_eq!(descriptors.mix_presentations.len(), 2);
    assert!(matches!(
        PresentationDecoder::new(&descriptors, 0, SoundSystem::A, &DefaultFactory),
        Err(DecodeError::InvalidDescriptors(_))
    ));

    let mut decoder =
        PresentationDecoder::new(&descriptors, 1, SoundSystem::A, &DefaultFactory).unwrap();
    for obu in ObuIter::new(&data).map(Result::unwrap) {
        decoder.process_obu(&obu).unwrap();
    }
    let mix = decoder.finish().unwrap();
    assert_eq!(mix.channels, 2);
    assert_eq!(mix.interleaved.len(), FRAME * 2);
}

/// Streaming driver: automatic and by-id selection skip the empty mix;
/// an explicit index to it, or a stream with nothing else, is an error.
#[test]
fn empty_sub_mix_streaming_skipped_or_rejected() {
    let decode = |data: &[u8], selection: MixSelection| -> Result<(u32, usize), DecodeError> {
        let mut settings = StreamSettings::default();
        settings.mix_selection = selection;
        let mut decoder = StreamDecoder::new_from_descriptors(data, settings, &DefaultFactory)?;
        decoder.decode(data)?;
        let mut bytes = 0;
        while let Some(unit) = decoder.get_output_temporal_unit()? {
            bytes += unit.len();
        }
        Ok((decoder.selected_mix().0, bytes))
    };
    let unit_bytes = FRAME * 2 * 2;
    let both = empty_sub_mix_stream(true);
    assert_eq!(decode(&both, MixSelection::Auto).unwrap(), (8, unit_bytes));
    // Like any unsupported mix, a by-id request falls back to automatic
    // selection (iamf-tools RequestedMix semantics).
    assert_eq!(
        decode(&both, MixSelection::ById(7)).unwrap(),
        (8, unit_bytes)
    );
    assert_eq!(
        decode(&both, MixSelection::ByIndex(1)).unwrap(),
        (8, unit_bytes)
    );
    assert!(matches!(
        decode(&both, MixSelection::ByIndex(0)),
        Err(DecodeError::InvalidDescriptors(_))
    ));

    let only_empty = empty_sub_mix_stream(false);
    for selection in [
        MixSelection::Auto,
        MixSelection::ById(7),
        MixSelection::ByIndex(0),
    ] {
        assert!(
            matches!(
                decode(&only_empty, selection),
                Err(DecodeError::InvalidDescriptors(_))
            ),
            "{selection:?}"
        );
    }

    // Reconfiguring onto the empty mix fails the same way.
    let mut decoder =
        StreamDecoder::new_from_descriptors(&both, StreamSettings::default(), &DefaultFactory)
            .unwrap();
    assert!(matches!(
        decoder.reset_with_new_mix(MixSelection::ByIndex(0), None, &DefaultFactory),
        Err(DecodeError::InvalidDescriptors(_))
    ));
}

/// Layout bytes for [`single_element_mix`].
const SS_A: u8 = 0x80; // layout_type 2 (ss convention), sound system A
const SS_J: u8 = 0x80 | (9 << 2); // sound system J (7.1.4)
const BINAURAL_LAYOUT: u8 = 0xc0; // layout_type 3

/// Stereo element 1 content (constant) and binaural element 2 content
/// (distinct ramps), interleaved L/R.
fn stereo_frames() -> Vec<i16> {
    (0..FRAME).flat_map(|_| [8192i16, -8192]).collect()
}

fn binaural_frames() -> Vec<i16> {
    (0..FRAME as i16)
        .flat_map(|k| [k * 311 - 9000, 7000 - k * 97])
        .collect()
}

/// A stream with stereo element 1 and binaural-input element 2 (one unit
/// each) and the given mix presentation OBUs.
fn two_element_stream(mixes: &[Vec<u8>]) -> Vec<u8> {
    let mut stream = obu(31, b"iamf\x00\x01");
    stream.extend(lpcm_codec_config());
    stream.extend(single_layer_element(1, 1)); // stereo
    stream.extend(single_layer_element(2, 9)); // binaural
    for mix in mixes {
        stream.extend(mix);
    }
    stream.extend(frame_obu(1, &stereo_frames()));
    stream.extend(frame_obu(2, &binaural_frames()));
    stream
}

fn s16le(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// Selected mix id via the batch helper, checking StreamDecoder agrees.
fn selected_mix_id(stream: &[u8], selection: MixSelection, layout: SoundSystem) -> u32 {
    let descriptors = Descriptors::collect(stream).unwrap();
    let index = descriptors
        .select_mix_presentation(selection, layout, ProfileSet::default())
        .unwrap();
    let id = descriptors.mix_presentations[index].mix_presentation_id;
    let mut settings = StreamSettings::default();
    settings.layout = Some(layout);
    settings.mix_selection = selection;
    let streamed = StreamDecoder::new_from_descriptors(stream, settings, &DefaultFactory)
        .unwrap()
        .selected_mix()
        .0;
    assert_eq!(streamed, id, "{layout:?}: stream/batch selection mismatch");
    id
}

/// Batch (PresentationDecoder) decode of the auto-selected mix, as s16le.
fn batch_decode(stream: &[u8], layout: SoundSystem) -> Vec<u8> {
    let descriptors = Descriptors::collect(stream).unwrap();
    let index = descriptors
        .select_mix_presentation(MixSelection::Auto, layout, ProfileSet::all_v1())
        .unwrap();
    let mut decoder =
        PresentationDecoder::new(&descriptors, index, layout, &DefaultFactory).unwrap();
    for obu in ObuIter::new(stream).map(Result::unwrap) {
        decoder.process_obu(&obu).unwrap();
    }
    let mix = decoder.finish().unwrap();
    let samples: Vec<i16> = mix
        .interleaved
        .iter()
        .map(|&s| iamf_dec::post::quantize_s16(s))
        .collect();
    s16le(&samples)
}

/// §7.4.1 end to end: stereo output picks the stereo-rendering 2.2.1 mix
/// over an earlier binaural-rendering (2.2.2) one, and binaural output
/// picks the 2.1.1 binaural-input mix, whose element passes through
/// bit-exactly (no HRTF, although its headphones_rendering_mode is 1).
#[test]
fn creator_preferred_mix_selection() {
    let stream = two_element_stream(&[
        single_element_mix(10, 1, 1, SS_A),
        single_element_mix(20, 1, 0, SS_A),
        single_element_mix(30, 2, 1, SS_A),
    ]);
    assert_eq!(
        selected_mix_id(&stream, MixSelection::Auto, SoundSystem::A),
        20
    );
    assert_eq!(
        selected_mix_id(&stream, MixSelection::Auto, SoundSystem::Binaural),
        30
    );
    // 2.3.1 finds no 5.1 mix: first supported.
    assert_eq!(
        selected_mix_id(&stream, MixSelection::Auto, SoundSystem::B),
        10
    );

    assert_eq!(
        decode_all(&stream, SoundSystem::A).unwrap(),
        s16le(&stereo_frames())
    );
    let binaural = s16le(&binaural_frames());
    assert_eq!(
        decode_all(&stream, SoundSystem::Binaural).unwrap(),
        binaural
    );
    assert_eq!(batch_decode(&stream, SoundSystem::Binaural), binaural);
}

/// Binaural input passes through to binaural and stereo output (iamf-tools
/// `AudioElementRendererPassThrough`), and mixes containing it are skipped
/// for other loudspeaker layouts, which it cannot be rendered to.
#[test]
fn binaural_input_mix_passthrough_and_skipping() {
    let stream = two_element_stream(&[
        single_element_mix(30, 2, 1, BINAURAL_LAYOUT),
        single_element_mix(10, 1, 0, SS_J),
    ]);
    let binaural = s16le(&binaural_frames());
    // Binaural: 2.1.1. Stereo: no 2.2.x candidate, first mix (renderable
    // to stereo by passthrough).
    for layout in [SoundSystem::Binaural, SoundSystem::A] {
        assert_eq!(selected_mix_id(&stream, MixSelection::Auto, layout), 30);
        assert_eq!(decode_all(&stream, layout).unwrap(), binaural, "{layout:?}");
        assert_eq!(batch_decode(&stream, layout), binaural, "{layout:?}");
    }
    // 7.1.4: 2.3.1. 5.1: the first mix cannot render, so the first
    // renderable one; a requested id naming it falls back likewise.
    assert_eq!(
        selected_mix_id(&stream, MixSelection::Auto, SoundSystem::J),
        10
    );
    assert_eq!(
        selected_mix_id(&stream, MixSelection::Auto, SoundSystem::B),
        10
    );
    assert_eq!(
        selected_mix_id(&stream, MixSelection::ById(30), SoundSystem::B),
        10
    );
    assert_eq!(
        decode_all(&stream, SoundSystem::B).unwrap().len(),
        FRAME * 6 * 2
    );
    // Explicitly indexing the unrenderable mix is an error.
    let descriptors = Descriptors::collect(&stream).unwrap();
    assert!(matches!(
        descriptors.select_mix_presentation(
            MixSelection::ByIndex(0),
            SoundSystem::B,
            ProfileSet::all_v1()
        ),
        Err(DecodeError::UnsupportedProfile(_))
    ));
}

/// Mix presentation `id` with two sub mixes, each holding `element` and
/// one stereo layout entry: allowed only by the IAMF v2.0 profiles.
fn two_sub_mix_presentation(id: u32, element: u32) -> Vec<u8> {
    let mut p = leb(id);
    p.extend(leb(0)); // count_label
    p.extend(leb(2)); // num_sub_mixes
    for _ in 0..2 {
        p.extend(leb(1)); // num_audio_elements
        p.extend(leb(element));
        p.push(0x00); // headphones_rendering_mode
        p.extend(leb(0)); // rendering_config_extension_size
        p.extend(mix_gain_param(100 + element));
        p.extend(mix_gain_param(200 + id));
        p.extend(leb(1)); // num_layouts
        p.push(SS_A);
        p.push(0x00); // loudness info_type
        p.extend(0i16.to_be_bytes());
        p.extend(0i16.to_be_bytes());
    }
    obu(2, &p)
}

/// A stereo stream (one unit) whose sequence header declares
/// `profiles` (primary, additional) and carries the given mixes.
fn stereo_stream(profiles: [u8; 2], mixes: &[Vec<u8>]) -> Vec<u8> {
    let mut header = b"iamf".to_vec();
    header.extend(profiles);
    let mut stream = obu(31, &header);
    stream.extend(lpcm_codec_config());
    stream.extend(stereo_element());
    for mix in mixes {
        stream.extend(mix);
    }
    stream.extend(frame_obu(0, &stereo_frames()));
    stream
}

/// Like iamf-tools `IamfDecoder::Settings`, the default requested profiles
/// are the IAMF v1.1 ones: a mix only the v2.0 profiles allow (two sub
/// mixes) is skipped in favor of a later v1.1 mix.
#[test]
fn default_profiles_skip_v2_only_mix() {
    let stream = stereo_stream(
        [0, 1],
        &[two_sub_mix_presentation(5, 1), mix_presentation_of(8, &[1])],
    );
    assert_eq!(
        selected_mix_id(&stream, MixSelection::Auto, SoundSystem::A),
        8
    );
    assert_eq!(
        decode_all(&stream, SoundSystem::A).unwrap(),
        s16le(&stereo_frames())
    );
    // Requesting every profile makes the v2.0 mix selectable.
    let descriptors = Descriptors::collect(&stream).unwrap();
    assert_eq!(
        descriptors
            .select_mix_presentation(MixSelection::Auto, SoundSystem::A, ProfileSet::all())
            .unwrap(),
        0
    );
}

/// A stream declaring only IAMF v2.0 profiles is refused by default, and
/// decodes once the caller opts in to them.
#[test]
fn v2_only_sequence_header_needs_opt_in() {
    // Base-Advanced primary, Advanced1 additional.
    let stream = stereo_stream([3, 4], &[mix_presentation_of(8, &[1])]);
    assert!(matches!(
        decode_all(&stream, SoundSystem::A),
        Err(DecodeError::UnsupportedProfile(_))
    ));
    let mut settings = StreamSettings::default();
    settings.requested_profiles = ProfileSet::all();
    let mut decoder =
        StreamDecoder::new_from_descriptors(&stream, settings, &DefaultFactory).unwrap();
    decoder.decode(&stream).unwrap();
    let mut out = Vec::new();
    while let Some(unit) = decoder.get_output_temporal_unit().unwrap() {
        out.extend(unit);
    }
    assert_eq!(out, s16le(&stereo_frames()));
}

/// What [`FakeFactory`] saw: one entry per created renderer, and one per
/// rendered temporal unit.
#[derive(Debug, Default)]
struct RendererLog {
    /// (target, sample_rate, frame_size, elements) per created renderer.
    configs: Vec<(SoundSystem, u32, u32, Vec<ElementInfo>)>,
    units: Vec<UnitRecord>,
}

/// One rendered temporal unit, as [`FakeRenderer`] saw it.
#[derive(Debug)]
struct UnitRecord {
    ids: Vec<u32>,
    demixing_modes: Vec<Option<u8>>,
    plane_counts: Vec<usize>,
    len: usize,
    /// Sum of the first element's first plane.
    first_plane_sum: f32,
}

/// An external renderer that records its inputs and renders every output
/// channel as a constant 0.25.
struct FakeFactory {
    log: Arc<Mutex<RendererLog>>,
    refuse: bool,
}

struct FakeRenderer {
    log: Arc<Mutex<RendererLog>>,
    channels: usize,
}

impl MixRendererFactory for FakeFactory {
    fn create(&self, config: &MixConfig<'_>) -> Result<Box<dyn MixRenderer>, DecodeError> {
        self.log.lock().unwrap().configs.push((
            config.target,
            config.sample_rate,
            config.frame_size,
            config.elements.to_vec(),
        ));
        if self.refuse {
            return Err(DecodeError::UnsupportedRenderer("refused".into()));
        }
        Ok(Box::new(FakeRenderer {
            log: Arc::clone(&self.log),
            channels: config.target.channels(),
        }))
    }
}

impl MixRenderer for FakeRenderer {
    fn render(&mut self, elements: &[ElementFrame<'_>]) -> Result<Vec<Vec<f32>>, DecodeError> {
        let len = elements[0].planes[0].len();
        self.log.lock().unwrap().units.push(UnitRecord {
            ids: elements.iter().map(|e| e.audio_element_id).collect(),
            demixing_modes: elements.iter().map(|e| e.demixing_mode).collect(),
            plane_counts: elements.iter().map(|e| e.planes.len()).collect(),
            len,
            first_plane_sum: elements[0].planes[0].iter().sum(),
        });
        Ok(vec![vec![0.25; len]; self.channels])
    }
}

fn fake_decoder(
    data: &[u8],
    layout: SoundSystem,
    refuse: bool,
) -> (StreamDecoder, Arc<Mutex<RendererLog>>) {
    let log = Arc::new(Mutex::new(RendererLog::default()));
    let mut settings = StreamSettings::default();
    settings.layout = Some(layout);
    // headphones_rendering_mode 2 (head-locked) is allowed only by the IAMF
    // v2.0 profiles, which the default request leaves out.
    settings.requested_profiles = ProfileSet::all();
    let decoder = StreamDecoder::new_from_descriptors_with_renderer(
        data,
        settings,
        &DefaultFactory,
        Arc::new(FakeFactory {
            log: Arc::clone(&log),
            refuse,
        }),
    )
    .unwrap();
    (decoder, log)
}

/// Stereo + 5.1 scalable element (demixing parameter 7) in a one-element
/// mix with `rendering_mode`, followed by one temporal unit.
fn scalable_stream(rendering_mode: u8, layout_byte: u8) -> Vec<u8> {
    let mut data = descriptors(
        scalable_element(7),
        single_element_mix(42, 1, rendering_mode, layout_byte),
    );
    data.extend(demix_block(7, FRAME as u32, &[2]));
    push_unit(&mut data, 0);
    data
}

/// Layer selection for an external renderer: binaural output with
/// headphones_rendering_mode 1 (world-locked) or 2 (head-locked) hands it
/// the highest layer (5.1 here), whatever the `binaural` feature; mode 0
/// and loudspeaker targets select like the builtin renderer.
#[test]
fn external_renderer_layer_selection() {
    for (layout, mode, expected_layout, expected_planes) in [
        (SoundSystem::Binaural, 0, 1, 2),
        (SoundSystem::Binaural, 1, 2, 6),
        (SoundSystem::Binaural, 2, 2, 6),
        (SoundSystem::A, 1, 1, 2),
        (SoundSystem::B, 0, 2, 6),
    ] {
        let data = scalable_stream(mode, BINAURAL_LAYOUT);
        let (mut decoder, log) = fake_decoder(&data, layout, false);
        decoder.decode(&data).unwrap();
        let out = decoder.get_output_temporal_unit().unwrap().unwrap();
        assert_eq!(
            out.len(),
            FRAME * layout.channels() * 2,
            "{layout:?} mode {mode}"
        );

        let log = log.lock().unwrap();
        let [(target, sample_rate, frame_size, elements)] = log.configs.as_slice() else {
            panic!("expected one renderer, got {:?}", log.configs);
        };
        assert_eq!(*target, layout);
        assert_eq!(*sample_rate, 48000);
        assert_eq!(*frame_size, FRAME as u32);
        let [element] = elements.as_slice() else {
            panic!("expected one element");
        };
        assert_eq!(element.audio_element_id, 1);
        assert_eq!(element.headphones_rendering_mode, mode);
        assert_eq!(element.default_demixing_mode, Some(0));
        assert_eq!(
            element.layout,
            ElementLayout::Channels {
                loudspeaker_layout: expected_layout
            },
            "{layout:?} mode {mode}"
        );
        let [unit] = log.units.as_slice() else {
            panic!("expected one unit");
        };
        assert_eq!(unit.ids, [1]);
        assert_eq!(unit.demixing_modes, [Some(2)]);
        assert_eq!(unit.plane_counts, [expected_planes]);
        assert_eq!(unit.len, FRAME);
    }
}

/// The external renderer's planes become the output (after output gain,
/// trimming and quantization), the element mix gain is applied to its
/// inputs, refusals surface from `get_output_temporal_unit`, and a reset
/// creates a fresh renderer.
#[test]
fn external_renderer_output_gain_and_lifecycle() {
    let head = descriptors(stereo_element(), single_element_mix(42, 1, 0, SS_A));
    let frames: Vec<i16> = (0..FRAME).flat_map(|_| [8192i16, -8192]).collect();
    let mut plain = head.clone();
    plain.extend(frame_obu(0, &frames));
    let mut gained = head.clone();
    gained.extend(mix_gain_block(100, FRAME as u32, -256 * 6)); // -6 dB
    gained.extend(frame_obu(0, &frames));

    let render = |data: &[u8]| {
        let (mut decoder, log) = fake_decoder(data, SoundSystem::A, false);
        decoder.decode(data).unwrap();
        let out = decoder.get_output_temporal_unit().unwrap().unwrap();
        let sum = log.lock().unwrap().units[0].first_plane_sum;
        (out, sum)
    };
    let (out, plain_sum) = render(&plain);
    let expected = iamf_dec::post::quantize_s16(0.25).to_le_bytes();
    assert_eq!(out.len(), FRAME * 2 * 2);
    assert!(out.chunks_exact(2).all(|s| s == expected));
    let (_, gained_sum) = render(&gained);
    let ratio = gained_sum / plain_sum;
    assert!(
        (ratio - 10f32.powf(-6.0 / 20.0)).abs() < 1e-3,
        "ratio {ratio}"
    );

    // Refusal, then a reset retries with a fresh renderer.
    let (mut decoder, log) = fake_decoder(&plain, SoundSystem::A, true);
    decoder.decode(&plain).unwrap();
    assert!(matches!(
        decoder.get_output_temporal_unit(),
        Err(DecodeError::UnsupportedRenderer(_))
    ));
    decoder.reset();
    decoder.decode(&plain).unwrap();
    assert!(decoder.get_output_temporal_unit().is_err());
    assert_eq!(log.lock().unwrap().configs.len(), 2);

    // reset_with_new_mix keeps using the external renderer.
    let (mut decoder, log) = fake_decoder(&plain, SoundSystem::A, false);
    decoder
        .reset_with_new_mix(MixSelection::Auto, Some(SoundSystem::B), &DefaultFactory)
        .unwrap();
    decoder.decode(&plain).unwrap();
    let out = decoder.get_output_temporal_unit().unwrap().unwrap();
    assert_eq!(out.len(), FRAME * 6 * 2);
    assert_eq!(log.lock().unwrap().configs[0].0, SoundSystem::B);
}

/// Regression: demixing (and recon-gain) blocks had no gap filling. With
/// blocks for units 0 and 2 only, unit 1 took unit 2's mode when the
/// whole stream was pushed before pulling (and in the batch decoder), but
/// kept unit 0's when pulling after every push. The previous value now
/// stays in effect over the gap in every driver and chunking.
#[test]
fn demix_gap_keeps_previous_mode_under_any_chunking() {
    let head = descriptors(scalable_element(7), mix_presentation(100, 101));
    let mut units = Vec::new();
    for unit in 0..3 {
        let mut bytes = Vec::new();
        match unit {
            0 => bytes.extend(demix_block(7, FRAME as u32, &[1])),
            2 => bytes.extend(demix_block(7, FRAME as u32, &[2])),
            _ => {}
        }
        push_unit(&mut bytes, unit);
        units.push(bytes);
    }
    let whole: Vec<u8> = head.iter().copied().chain(units.concat()).collect();

    // Push everything, then pull.
    let all_at_once = decode_all(&whole, SoundSystem::B).unwrap();

    // Pull after every unit.
    let mut settings = StreamSettings::default();
    settings.layout = Some(SoundSystem::B);
    let mut decoder =
        StreamDecoder::new_from_descriptors(&head, settings, &DefaultFactory).unwrap();
    let mut incremental = Vec::new();
    for unit in &units {
        decoder.decode(unit).unwrap();
        while let Some(bytes) = decoder.get_output_temporal_unit().unwrap() {
            incremental.extend(bytes);
        }
    }
    assert_eq!(all_at_once, incremental, "output depends on chunking");

    // Unit 1 keeps mode 1: same as a stream that repeats unit 0's block.
    let mut explicit = head.clone();
    for (unit, mode) in [(0, 1), (1, 1), (2, 2)] {
        explicit.extend(demix_block(7, FRAME as u32, &[mode]));
        push_unit(&mut explicit, unit);
    }
    assert_eq!(all_at_once, decode_all(&explicit, SoundSystem::B).unwrap());

    // The batch decoder agrees.
    let descriptors = Descriptors::collect(&whole).unwrap();
    let mut batch =
        PresentationDecoder::new(&descriptors, 0, SoundSystem::B, &DefaultFactory).unwrap();
    for obu in ObuIter::new(&whole) {
        batch.process_obu(&obu.unwrap()).unwrap();
    }
    let mix = batch.finish().unwrap();
    let batch_bytes: Vec<u8> = mix
        .interleaved
        .iter()
        .flat_map(|&s| iamf_dec::post::quantize_s16(s).to_le_bytes())
        .collect();
    assert_eq!(batch_bytes, all_at_once, "batch differs from stream");
}
