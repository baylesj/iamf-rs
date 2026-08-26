//! HRTF binaural conformance against obr (via iamf-tools' decoder):
//! reference WAVs in tests/data were produced by
//! `decoder_main --output_layout Binaural` from mode-1 variants of the
//! fetched vectors; the same variants are recreated here by patching
//! `headphones_rendering_mode` to 1 in the mix presentation OBU.

mod common;

use std::path::PathBuf;

use iamf_codecs::DefaultFactory;
use iamf_dec::layout::SoundSystem;
use iamf_dec::presentation::{Descriptors, PresentationDecoder};
use iamf_dec::stream::{StreamDecoder, StreamSettings};
use iamf_obu::{ByteReader, Obu, ObuIter, ObuType};

use common::vectors_dir;

fn data_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/data")
}

/// Sets headphones_rendering_mode = 1 (BINAURAL) for the first element of
/// the first sub mix, in place. The rendering byte's file offset is found
/// by walking the mix presentation payload.
fn set_binaural_mode(data: &mut [u8]) {
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

fn read_wav_s16(path: &std::path::Path) -> Vec<i16> {
    let data = std::fs::read(path).unwrap();
    let mut pos = 12;
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let size = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap()) as usize;
        if id == b"data" {
            return data[pos + 8..pos + 8 + size]
                .chunks_exact(2)
                .map(|b| i16::from_le_bytes([b[0], b[1]]))
                .collect();
        }
        pos += 8 + size + (size & 1);
    }
    panic!("no data chunk");
}

fn decode_binaural(data: &[u8]) -> Vec<i16> {
    let descriptors = Descriptors::collect(data).unwrap();
    let mut decoder =
        PresentationDecoder::new(&descriptors, 0, SoundSystem::Binaural, &DefaultFactory).unwrap();
    for obu in ObuIter::new(data).map(Result::unwrap) {
        decoder.process_obu(&obu).unwrap();
    }
    let mix = decoder.finish().unwrap();
    mix.interleaved
        .iter()
        .map(|&s| iamf_dec::post::quantize_s16(s))
        .collect()
}

fn hrtf_case(vector: &str, reference: &str, tolerance: i32) {
    let mut data = require_vectors!(
        std::fs::read(vectors_dir().join(format!("{vector}.iamf"))).ok(),
        format_args!("{vector}")
    );
    set_binaural_mode(&mut data);
    let ours = decode_binaural(&data);
    let reference = read_wav_s16(&data_dir().join(reference));
    assert_eq!(ours.len(), reference.len(), "{vector}: sample count");
    let max_diff = ours
        .iter()
        .zip(&reference)
        .map(|(&a, &b)| (i32::from(a) - i32::from(b)).abs())
        .max()
        .unwrap_or(0);
    assert!(
        max_diff <= tolerance,
        "{vector}: max diff {max_diff} exceeds {tolerance}"
    );
}

#[test]
fn hrtf_714_matches_obr() {
    hrtf_case("test_000070", "test_000070_mode1_binaural_ref.wav", 4);
}

#[test]
fn hrtf_foa_matches_obr() {
    hrtf_case("test_000038", "test_000038_mode1_binaural_ref.wav", 4);
}

/// Streaming binaural must match the batch pipeline byte-for-byte.
#[test]
fn hrtf_stream_matches_batch() {
    let mut data = require_vectors!(
        std::fs::read(vectors_dir().join("test_000070.iamf")).ok(),
        "test_000070"
    );
    set_binaural_mode(&mut data);
    let batch: Vec<u8> = decode_binaural(&data)
        .iter()
        .flat_map(|s| s.to_le_bytes())
        .collect();

    let mut settings = StreamSettings::default();
    settings.layout = SoundSystem::Binaural;
    let mut decoder =
        StreamDecoder::new_from_descriptors(&data, settings, &DefaultFactory).unwrap();
    let mut streamed = Vec::new();
    for chunk in data.chunks(4096) {
        decoder.decode(chunk).unwrap();
        while decoder.is_temporal_unit_available() {
            streamed.extend(decoder.get_output_temporal_unit().unwrap().unwrap());
        }
    }
    assert_eq!(streamed.len(), batch.len());
    assert!(streamed == batch, "stream/batch binaural mismatch");
}
