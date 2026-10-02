//! IAMF v2.0 object passthrough on the libiamf object vectors
//! (`test_0008xx` base-advanced, `test_0009xx` advanced-1). They are all
//! LPCM, so an object's PCM must equal its source exactly, and the
//! positions follow the parameter blocks and defaults their textprotos
//! spell out.

mod common;

use iamf_codecs::DefaultFactory;
use iamf_dec::DecodeError;
use iamf_dec::layout::SoundSystem;
use iamf_dec::params::q78_db_to_linear;
use iamf_dec::position::ObjectPosition;
use iamf_dec::stream::{DecodedObject, MixSelection, StreamDecoder, StreamSettings};

fn vector(name: &str) -> Option<Vec<u8>> {
    std::fs::read(common::vectors_dir().join(format!("{name}.iamf"))).ok()
}

/// 16-bit WAV samples per channel, as the LPCM decoder scales them.
fn wav_channels(name: &str) -> Option<Vec<Vec<f32>>> {
    let bytes = std::fs::read(common::vectors_dir().join(name)).ok()?;
    let mut pos = 12;
    let mut channels = 0usize;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = &bytes[pos + 8..(pos + 8 + len).min(bytes.len())];
        if id == b"fmt " {
            channels = usize::from(u16::from_le_bytes([body[2], body[3]]));
            assert_eq!(u16::from_le_bytes([body[14], body[15]]), 16);
        } else if id == b"data" {
            let mut out = vec![Vec::new(); channels];
            for (i, s) in body.chunks_exact(2).enumerate() {
                out[i % channels].push(f32::from(i16::from_le_bytes([s[0], s[1]])) / 32768.0);
            }
            return Some(out);
        }
        pos += 8 + len + (len & 1);
    }
    None
}

struct Decoded {
    /// Per temporal unit: the rendered bed bytes and the objects.
    units: Vec<(Vec<u8>, Vec<DecodedObject>)>,
    num_objects: usize,
    has_rendered_elements: bool,
}

fn decode(data: &[u8], layout: SoundSystem, passthrough: bool) -> Result<Decoded, DecodeError> {
    let mut settings = StreamSettings::default();
    settings.layout = layout;
    settings.mix_selection = MixSelection::Auto;
    settings.object_passthrough = passthrough;
    settings.object_position_interval = 512;
    let mut decoder = StreamDecoder::new_from_descriptors(data, settings, &DefaultFactory)?;
    let mut units = Vec::new();
    // Uneven chunks, as a streaming host delivers them.
    for chunk in data.chunks(997) {
        decoder.decode(chunk)?;
        while let Some(bed) = decoder.get_output_temporal_unit()? {
            units.push((bed, decoder.take_objects()));
        }
    }
    Ok(Decoded {
        units,
        num_objects: decoder.num_objects(),
        has_rendered_elements: decoder.has_rendered_elements(),
    })
}

fn polar(p: ObjectPosition) -> [f32; 3] {
    match p {
        ObjectPosition::Polar {
            azimuth,
            elevation,
            distance,
        } => [azimuth, elevation, distance],
        ObjectPosition::Cartesian { .. } => panic!("expected a polar position, got {p:?}"),
    }
}

fn cartesian(p: ObjectPosition) -> [f32; 3] {
    match p {
        ObjectPosition::Cartesian { x, y, z } => [x, y, z],
        ObjectPosition::Polar { .. } => panic!("expected a cartesian position, got {p:?}"),
    }
}

fn assert_close(actual: [f32; 3], expected: [f32; 3], what: &str) {
    for (a, e) in actual.iter().zip(expected) {
        assert!((a - e).abs() < 1e-3, "{what}: {actual:?} != {expected:?}");
    }
}

/// Position of object `o` at sample offset `at` of unit `u`.
fn position(d: &Decoded, u: usize, o: usize, at: u32) -> ObjectPosition {
    d.units[u].1[o]
        .positions
        .iter()
        .find(|(offset, _)| *offset == at)
        .unwrap_or_else(|| panic!("no position at {at} in unit {u}"))
        .1
}

#[test]
fn polar_object_is_its_source_and_follows_its_blocks() {
    let data = require_vectors!(vector("test_000800"), "test_000800");
    let source = require_vectors!(
        wav_channels("dialog_clip_stereo.wav"),
        "dialog_clip_stereo.wav"
    );
    let d = decode(&data, SoundSystem::A, true).unwrap();
    assert_eq!(d.num_objects, 1);
    assert!(
        !d.has_rendered_elements,
        "an object-only mix renders nothing"
    );

    // The PCM is the source's first channel through the element's -3 dB
    // mix gain (default_mix_gain -768), sample for sample.
    let gain = q78_db_to_linear(-768);
    let expected: Vec<f32> = source[0].iter().map(|&s| s * gain).collect();
    let pcm: Vec<f32> = d
        .units
        .iter()
        .flat_map(|(_, objects)| objects[0].samples.iter().copied())
        .collect();
    let n = pcm.len().min(expected.len());
    assert!(n > 100_000);
    assert_eq!(pcm[..n], expected[..n]);
    // Nothing is rendered into the bed.
    assert!(d.units.iter().all(|(bed, _)| bed.iter().all(|&b| b == 0)));

    // Front (step), then inter-linear along the horizon to the left,
    // the rear and the right; past the blocks, the default (front).
    assert_close(polar(position(&d, 0, 0, 0)), [0.0, 0.0, 1.0], "unit 0");
    assert_close(
        polar(position(&d, 1, 0, 512)),
        [45.0, 0.0, 1.0],
        "unit 1 mid",
    );
    assert_close(
        polar(position(&d, 2, 0, 0)),
        [90.0, 0.0, 1.0],
        "unit 2 start",
    );
    assert_close(
        polar(position(&d, 2, 0, 512)),
        [135.0, 0.0, 1.0],
        "unit 2 mid",
    );
    let rear = polar(position(&d, 3, 0, 0));
    assert!(
        (rear[0].abs() - 180.0).abs() < 1e-3,
        "unit 3 start {rear:?}"
    );
    // 180 → -90 is a 90° arc through the rear-right.
    assert_close(
        polar(position(&d, 3, 0, 512)),
        [-135.0, 0.0, 1.0],
        "unit 3 mid",
    );
    assert_close(
        polar(position(&d, 4, 0, 0)),
        [0.0, 0.0, 1.0],
        "unit 4 default",
    );
}

#[test]
fn cartesian_objects_interpolate_per_coordinate() {
    for name in ["test_000801", "test_000802"] {
        let data = require_vectors!(vector(name), name);
        let d = decode(&data, SoundSystem::A, true).unwrap();
        assert_close(cartesian(position(&d, 0, 0, 0)), [0.0, 1.0, 0.0], name);
        // Front (0, 1, 0) to left (-1, 0, 0), linearly per coordinate.
        assert_close(cartesian(position(&d, 1, 0, 512)), [-0.5, 0.5, 0.0], name);
        assert_close(cartesian(position(&d, 2, 0, 0)), [-1.0, 0.0, 0.0], name);
        assert_close(cartesian(position(&d, 3, 0, 0)), [0.0, -1.0, 0.0], name);
    }
}

#[test]
fn dual_objects_hold_their_defaults() {
    let data = require_vectors!(vector("test_000806"), "test_000806");
    let source = require_vectors!(
        wav_channels("dialog_clip_stereo.wav"),
        "dialog_clip_stereo.wav"
    );
    let d = decode(&data, SoundSystem::A, true).unwrap();
    assert_eq!(d.num_objects, 2);
    for (_, objects) in d.units.iter().step_by(37) {
        assert_eq!(objects.len(), 2);
        assert_eq!((objects[0].index, objects[1].index), (0, 1));
        assert_close(
            polar(objects[0].positions[0].1),
            [1.0, 2.0, 3.0 / 127.0],
            "first",
        );
        assert_close(
            polar(objects[1].positions[0].1),
            [4.0, 5.0, 6.0 / 127.0],
            "second",
        );
    }
    // Each object is one channel of the stereo source, through the
    // element's -3 dB mix gain.
    let gain = q78_db_to_linear(-768);
    for o in 0..2 {
        let pcm: Vec<f32> = d
            .units
            .iter()
            .flat_map(|(_, objs)| objs[o].samples.clone())
            .collect();
        let expected: Vec<f32> = source[o].iter().map(|&s| s * gain).collect();
        let n = pcm.len().min(expected.len());
        assert_eq!(pcm[..n], expected[..n], "object {o}");
    }
}

#[test]
fn mixed_mix_renders_the_bed_and_hands_out_the_objects() {
    // Advanced-1: a 5.1 channel-based element and four mono polar objects.
    let data = require_vectors!(vector("test_000903"), "test_000903");
    let d = decode(&data, SoundSystem::B, true).unwrap();
    assert_eq!(d.num_objects, 4);
    assert!(d.has_rendered_elements);
    assert!(d.units.iter().any(|(bed, _)| bed.iter().any(|&b| b != 0)));
    for (_, objects) in &d.units {
        assert_eq!(objects.len(), 4);
        for object in objects {
            assert_close(
                polar(object.positions[0].1),
                [1.0, 2.0, 3.0 / 127.0],
                "default",
            );
        }
    }
}

#[test]
fn objects_are_not_selectable_without_passthrough() {
    let data = require_vectors!(vector("test_000800"), "test_000800");
    // Base-advanced only, no v1.1 fallback mix: nothing this decoder can
    // render.
    match decode(&data, SoundSystem::A, false) {
        Err(DecodeError::UnsupportedProfile(_)) => {}
        other => panic!(
            "expected UnsupportedProfile, got {:?}",
            other.map(|d| d.units.len())
        ),
    }
}

#[test]
fn passthrough_leaves_v1_streams_unchanged() {
    let data = require_vectors!(vector("test_000082"), "test_000082");
    let plain = decode(&data, SoundSystem::J, false).unwrap();
    let pass = decode(&data, SoundSystem::J, true).unwrap();
    assert_eq!(pass.num_objects, 0);
    assert_eq!(plain.units.len(), pass.units.len());
    for ((a, oa), (b, ob)) in plain.units.iter().zip(&pass.units) {
        assert_eq!(a, b);
        assert!(oa.is_empty() && ob.is_empty());
    }
}
