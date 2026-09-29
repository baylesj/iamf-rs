//! Shared helpers for the vector-driven conformance tests.

use std::path::PathBuf;

use iamf_obu::{ByteReader, Obu, ObuType};

pub(crate) fn vectors_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/vectors")
}

/// Unwraps a vector-dependent resource. Missing vectors FAIL the test with
/// fetch instructions so absent fixtures can't masquerade as green runs;
/// set `IAMF_VECTORS_OPTIONAL=1` to skip (with a message) instead, e.g.
/// for offline environments.
#[macro_export]
macro_rules! require_vectors {
    ($opt:expr, $what:expr) => {
        match $opt {
            Some(v) => v,
            None if std::env::var_os("IAMF_VECTORS_OPTIONAL").is_some() => {
                eprintln!("SKIPPED: {} missing; run tools/fetch_vectors.sh", $what);
                return;
            }
            None => panic!(
                "{} missing; run tools/fetch_vectors.sh \
                 (or set IAMF_VECTORS_OPTIONAL=1 to skip vector tests)",
                $what
            ),
        }
    };
}

/// Sets headphones_rendering_mode = 1 (BINAURAL) for the first element of
/// the first sub mix, in place. The rendering byte's file offset is found
/// by walking the mix presentation payload.
#[allow(dead_code)] // not every test binary patches vectors
pub(crate) fn set_binaural_mode(data: &mut [u8]) {
    let mut mix_payload_range = None;
    {
        let mut reader = ByteReader::new(data);
        loop {
            let before = reader.position();
            let Ok(obu) = Obu::parse(&mut reader) else {
                break;
            };
            if obu.header.obu_type == ObuType::MixPresentation {
                let end = reader.position();
                let start = end - obu.payload.len();
                mix_payload_range = Some((start, end));
                break;
            }
            if before == reader.position() {
                break;
            }
        }
    }
    let (start, end) = mix_payload_range.expect("mix presentation present");
    let mut r = ByteReader::new(&data[start..end]);
    r.read_leb128().unwrap(); // mix_presentation_id
    let count_label = r.read_leb128().unwrap();
    for _ in 0..count_label * 2 {
        r.read_string().unwrap();
    }
    r.read_leb128().unwrap(); // num_sub_mixes
    r.read_leb128().unwrap(); // num_audio_elements
    r.read_leb128().unwrap(); // audio_element_id
    for _ in 0..count_label {
        r.read_string().unwrap();
    }
    let offset = start + r.position();
    data[offset] = (data[offset] & 0x3f) | 0x40;
}
