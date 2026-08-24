//! The streaming decoder must produce byte-identical output to the batch
//! presentation pipeline, including when the bitstream arrives in awkward
//! partial-OBU chunks (Chromium feeds arbitrary buffer sizes).

use std::path::PathBuf;

use iamf_codecs::DefaultFactory;
use iamf_dec::layout::SoundSystem;
use iamf_dec::presentation::{Descriptors, PresentationDecoder};
use iamf_dec::stream::{OutputSampleType, StreamDecoder, StreamSettings};
use iamf_obu::ObuIter;

fn vector(name: &str) -> Option<Vec<u8>> {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../../tests/vectors/{name}.iamf"));
    std::fs::read(path).ok()
}

/// Batch decode to s16le bytes (reference behavior, conformance-tested).
fn batch_decode(data: &[u8], sound_system: u8) -> Vec<u8> {
    let descriptors = Descriptors::collect(data).unwrap();
    let target = SoundSystem::from_u8(sound_system).unwrap();
    let mut decoder = PresentationDecoder::new(&descriptors, 0, target, &DefaultFactory).unwrap();
    for obu in ObuIter::new(data).map(Result::unwrap) {
        decoder.process_obu(&obu).unwrap();
    }
    let mix = decoder.finish().unwrap();
    let mut bytes = Vec::with_capacity(mix.interleaved.len() * 2);
    for &sample in &mix.interleaved {
        let scaled = (sample * 32768.0).clamp(-32768.0, 32767.0);
        bytes.extend((scaled.round_ties_even() as i16).to_le_bytes());
    }
    bytes
}

/// Streaming decode with a rotating pattern of chunk sizes, pulling units
/// as they become available.
fn stream_decode(data: &[u8], sound_system: u8, chunks: &[usize]) -> Vec<u8> {
    let settings = StreamSettings {
        layout: SoundSystem::from_u8(sound_system).unwrap(),
        sample_type: Some(OutputSampleType::Int16LittleEndian),
        mix_selection: iamf_dec::stream::MixSelection::ByIndex(0),
        ..StreamSettings::default()
    };
    let mut decoder = StreamDecoder::new_from_descriptors(data, settings, &DefaultFactory).unwrap();
    let mut out = Vec::new();
    let mut pos = 0usize;
    let mut chunk_index = 0usize;
    while pos < data.len() {
        let size = chunks[chunk_index % chunks.len()].min(data.len() - pos);
        chunk_index += 1;
        decoder.decode(&data[pos..pos + size]).unwrap();
        pos += size;
        while decoder.is_temporal_unit_available() {
            out.extend(decoder.get_output_temporal_unit().unwrap().unwrap());
        }
    }
    decoder.signal_end_of_decoding();
    while decoder.is_temporal_unit_available() {
        out.extend(decoder.get_output_temporal_unit().unwrap().unwrap());
    }
    out
}

fn equivalence_case(name: &str, sound_system: u8) {
    let Some(data) = vector(name) else {
        eprintln!("{name}: vector missing; run tools/fetch_vectors.sh");
        return;
    };
    let batch = batch_decode(&data, sound_system);
    for chunks in [&[usize::MAX][..], &[1024, 7, 3][..], &[1][..]] {
        // Single-byte feeding is slow; only use it for small vectors.
        if chunks == [1] && data.len() > 200_000 {
            continue;
        }
        let streamed = stream_decode(&data, sound_system, chunks);
        assert_eq!(
            streamed.len(),
            batch.len(),
            "{name} chunks {chunks:?}: length mismatch"
        );
        assert!(
            streamed == batch,
            "{name} chunks {chunks:?}: content mismatch"
        );
    }
}

#[test]
fn stream_matches_batch_lpcm_stereo() {
    equivalence_case("test_000002", 0);
}

#[test]
fn stream_matches_batch_opus() {
    equivalence_case("test_000026", 0);
}

#[test]
fn stream_matches_batch_ambisonics() {
    equivalence_case("test_000038", 0);
}

#[test]
fn stream_matches_batch_scalable_demix() {
    equivalence_case("test_000036", 1);
}

#[test]
fn stream_matches_batch_multi_element() {
    equivalence_case("test_000086", 2);
}

#[test]
fn stream_matches_batch_animated_gains() {
    equivalence_case("test_000066", 0);
}

#[test]
fn stream_reset_allows_redecode() {
    let Some(data) = vector("test_000002") else {
        return;
    };
    let settings = StreamSettings::default();
    let mut decoder =
        StreamDecoder::new_from_descriptors(&data, settings, &DefaultFactory).unwrap();
    decoder.decode(&data).unwrap();
    let mut first = Vec::new();
    while decoder.is_temporal_unit_available() {
        first.extend(decoder.get_output_temporal_unit().unwrap().unwrap());
    }
    decoder.reset();
    decoder.decode(&data).unwrap();
    let mut second = Vec::new();
    while decoder.is_temporal_unit_available() {
        second.extend(decoder.get_output_temporal_unit().unwrap().unwrap());
    }
    assert_eq!(first, second);
}

/// Android channel ordering permutes the interleaved output (7.1.4:
/// rears before sides).
#[test]
fn android_channel_ordering() {
    let Some(data) = vector("test_000070") else {
        return;
    };
    let decode = |ordering| {
        let settings = StreamSettings {
            layout: SoundSystem::from_u8(9).unwrap(),
            sample_type: Some(OutputSampleType::Int16LittleEndian),
            channel_ordering: ordering,
            ..StreamSettings::default()
        };
        let mut decoder =
            StreamDecoder::new_from_descriptors(&data, settings, &DefaultFactory).unwrap();
        decoder.decode(&data).unwrap();
        let mut out = Vec::new();
        while decoder.is_temporal_unit_available() {
            out.extend(decoder.get_output_temporal_unit().unwrap().unwrap());
        }
        out
    };
    let iamf = decode(iamf_dec::stream::ChannelOrdering::Iamf);
    let android = decode(iamf_dec::stream::ChannelOrdering::Android);
    assert_eq!(iamf.len(), android.len());
    // 12 channels x 2 bytes per frame; Android takes ch6/7 where IAMF has
    // ch4/5 and vice versa.
    let stride = 12 * 2;
    for frame in 0..64 {
        let base = frame * stride;
        for (android_slot, iamf_slot) in [(4, 6), (5, 7), (6, 4), (7, 5), (0, 0), (11, 11)] {
            assert_eq!(
                android[base + android_slot * 2..base + android_slot * 2 + 2],
                iamf[base + iamf_slot * 2..base + iamf_slot * 2 + 2],
                "frame {frame} slot {android_slot}"
            );
        }
    }
}
