//! The streaming decoder must produce byte-identical output to the batch
//! presentation pipeline, including when the bitstream arrives in awkward
//! partial-OBU chunks (Chromium feeds arbitrary buffer sizes).

mod common;

use iamf_codecs::DefaultFactory;
use iamf_dec::layout::SoundSystem;
use iamf_dec::presentation::{Descriptors, PresentationDecoder};
use iamf_dec::stream::{MixSelection, OutputSampleType, StreamDecoder, StreamSettings};
use iamf_obu::ObuIter;

fn vector(name: &str) -> Option<Vec<u8>> {
    std::fs::read(common::vectors_dir().join(format!("{name}.iamf"))).ok()
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
    let mut settings = StreamSettings::default();
    settings.layout = SoundSystem::from_u8(sound_system).unwrap();
    settings.sample_type = Some(OutputSampleType::Int16LittleEndian);
    settings.mix_selection = MixSelection::ByIndex(0);
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
    let data = require_vectors!(vector(name), format_args!("{name}"));
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
fn stream_matches_batch_projection() {
    equivalence_case("test_000048", 0);
}

/// Vectors marked `is_valid_to_decode: false` must be rejected, not
/// decoded on a best-effort basis: 000007 has a non-lowercase ia_code,
/// 000025 an Opus version of 16 (§3.6.1 requires 1).
#[test]
fn invalid_vectors_rejected() {
    for name in ["test_000007", "test_000025"] {
        let data = require_vectors!(vector(name), format_args!("{name}"));
        assert!(
            StreamDecoder::new_from_descriptors(&data, StreamSettings::default(), &DefaultFactory)
                .is_err(),
            "{name}: invalid stream was accepted"
        );
    }
}

/// In-place mix/layout switch (iamf-tools ResetWithNewMix): after
/// reconfiguring, the decoder must produce exactly what a fresh decoder
/// with those settings produces.
#[test]
fn reset_with_new_mix_matches_fresh_decoder() {
    let data = require_vectors!(vector("test_000070"), format_args!("test_000070"));
    let fresh = |sound_system: u8| {
        let mut settings = StreamSettings::default();
        settings.layout = SoundSystem::from_u8(sound_system).unwrap();
        let mut decoder =
            StreamDecoder::new_from_descriptors(&data, settings, &DefaultFactory).unwrap();
        decoder.decode(&data).unwrap();
        let mut out = Vec::new();
        while decoder.is_temporal_unit_available() {
            out.extend(decoder.get_output_temporal_unit().unwrap().unwrap());
        }
        out
    };

    let mut decoder =
        StreamDecoder::new_from_descriptors(&data, StreamSettings::default(), &DefaultFactory)
            .unwrap();
    decoder.decode(&data[..data.len() / 2]).unwrap();
    let (mix_id, layout) = decoder
        .reset_with_new_mix(
            MixSelection::Auto,
            Some(SoundSystem::from_u8(9).unwrap()),
            &DefaultFactory,
        )
        .unwrap();
    assert_eq!(layout, SoundSystem::from_u8(9).unwrap());
    assert_eq!(decoder.num_output_channels(), 12);
    assert_eq!(decoder.selected_mix(), (mix_id, layout));
    decoder.decode(&data).unwrap();
    let mut switched = Vec::new();
    while decoder.is_temporal_unit_available() {
        switched.extend(decoder.get_output_temporal_unit().unwrap().unwrap());
    }
    assert_eq!(switched, fresh(9), "7.1.4 after switch");

    // And back to stereo on the same handle.
    decoder
        .reset_with_new_mix(MixSelection::Auto, Some(SoundSystem::A), &DefaultFactory)
        .unwrap();
    decoder.decode(&data).unwrap();
    let mut back = Vec::new();
    while decoder.is_temporal_unit_available() {
        back.extend(decoder.get_output_temporal_unit().unwrap().unwrap());
    }
    assert_eq!(back, fresh(0), "stereo after switching back");
}

/// A quiet signal passes the limiter untouched (gain 1.0, delay
/// compensated), so enabling it must not change this vector's output.
#[test]
fn limiter_passthrough_below_threshold() {
    let data = require_vectors!(vector("test_000002"), format_args!("test_000002"));
    let decode = |enable_limiter: bool| {
        let mut settings = StreamSettings::default();
        settings.enable_limiter = enable_limiter;
        let mut decoder =
            StreamDecoder::new_from_descriptors(&data, settings, &DefaultFactory).unwrap();
        decoder.decode(&data).unwrap();
        let mut out = Vec::new();
        while decoder.is_temporal_unit_available() {
            out.extend(decoder.get_output_temporal_unit().unwrap().unwrap());
        }
        out
    };
    assert_eq!(decode(false), decode(true));
}

/// Loudness normalization applies a constant gain of target - content;
/// a target equal to the stream's integrated loudness is a no-op, and a
/// -6 dB offset scales samples by 10^(-6/20).
#[test]
fn loudness_normalization_gain() {
    let data = require_vectors!(vector("test_000002"), format_args!("test_000002"));
    let descriptors = Descriptors::collect(&data).unwrap();
    let content_db = f32::from(
        descriptors.mix_presentations[0].sub_mixes[0].layouts[0]
            .1
            .integrated_loudness,
    ) / 256.0;
    let decode = |target: Option<f32>| {
        let mut settings = StreamSettings::default();
        settings.loudness_target_db = target;
        settings.sample_type = Some(OutputSampleType::Int32LittleEndian);
        let mut decoder =
            StreamDecoder::new_from_descriptors(&data, settings, &DefaultFactory).unwrap();
        decoder.decode(&data).unwrap();
        let mut out = Vec::new();
        while decoder.is_temporal_unit_available() {
            out.extend(decoder.get_output_temporal_unit().unwrap().unwrap());
        }
        out.chunks_exact(4)
            .map(|b| i32::from_le_bytes(b.try_into().unwrap()))
            .collect::<Vec<i32>>()
    };
    assert_eq!(decode(None), decode(Some(content_db)), "no-op target");
    let plain = decode(None);
    let attenuated = decode(Some(content_db - 6.0));
    let gain = 10f32.powf(-6.0 / 20.0);
    for (&a, &p) in attenuated.iter().zip(&plain) {
        let expected = (f64::from(p) * f64::from(gain)).round() as i64;
        assert!(
            (i64::from(a) - expected).abs() <= 1,
            "sample {a} vs expected {expected}"
        );
    }
}

#[test]
fn stream_reset_allows_redecode() {
    let data = require_vectors!(vector("test_000002"), format_args!("test_000002"));
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
    let data = require_vectors!(vector("test_000070"), format_args!("test_000070"));
    let decode = |ordering| {
        let mut settings = StreamSettings::default();
        settings.layout = SoundSystem::from_u8(9).unwrap();
        settings.channel_ordering = ordering;
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
