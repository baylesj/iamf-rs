//! Decode benchmarks over real libiamf test vectors (fetch with
//! tools/fetch_vectors.sh; vectors that are missing are skipped).
//!
//! Throughput is reported in audio samples produced per second
//! (frames x output channels), so `Melem/s / (sample_rate x channels)`
//! gives the realtime multiple.

use std::path::PathBuf;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use iamf_codecs::DefaultFactory;
use iamf_dec::layout::SoundSystem;
use iamf_dec::presentation::{Descriptors, PresentationDecoder};
use iamf_dec::stream::{StreamDecoder, StreamSettings};
use iamf_obu::{ObuIter, descriptors};

struct Case {
    /// Vector base name.
    name: &'static str,
    /// Human label: content -> target layout.
    label: &'static str,
    /// Target sound system.
    sound_system: u8,
}

const CASES: &[Case] = &[
    Case {
        name: "test_000002",
        label: "lpcm_stereo_to_stereo",
        sound_system: 0,
    },
    Case {
        name: "test_000026",
        label: "opus_stereo_to_stereo",
        sound_system: 0,
    },
    Case {
        name: "test_000070",
        label: "lpcm_714_to_stereo",
        sound_system: 0,
    },
    Case {
        name: "test_000070",
        label: "lpcm_714_passthrough",
        sound_system: 9,
    },
    Case {
        name: "test_000038",
        label: "lpcm_foa_to_stereo",
        sound_system: 0,
    },
    Case {
        name: "test_000086",
        label: "lpcm_multi_element_demix",
        sound_system: 2,
    },
];

fn vector(name: &str) -> Option<Vec<u8>> {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../../tests/vectors/{name}.iamf"));
    std::fs::read(path).ok()
}

/// Output samples (frames x channels) a full decode of `data` produces.
fn output_samples(data: &[u8], sound_system: u8) -> u64 {
    let descriptors = Descriptors::collect(data).unwrap();
    let target = SoundSystem::from_u8(sound_system).unwrap();
    let mut decoder = PresentationDecoder::new(&descriptors, 0, target, &DefaultFactory).unwrap();
    for obu in ObuIter::new(data).map(Result::unwrap) {
        decoder.process_obu(&obu).unwrap();
    }
    decoder.finish().unwrap().interleaved.len() as u64
}

fn bench_obu_walk(c: &mut Criterion) {
    let Some(data) = vector("test_000070") else {
        eprintln!("vectors missing; run tools/fetch_vectors.sh");
        return;
    };
    let mut group = c.benchmark_group("parse");
    group.throughput(Throughput::Bytes(data.len() as u64));
    group.bench_function("obu_walk", |b| {
        b.iter(|| {
            let mut frames = 0usize;
            for obu in ObuIter::new(&data).map(Result::unwrap) {
                frames += usize::from(obu.header.obu_type.is_audio_frame());
            }
            frames
        })
    });
    group.bench_function("descriptors", |b| {
        b.iter(|| {
            let mut count = 0usize;
            for obu in ObuIter::new(&data).map(Result::unwrap) {
                if descriptors::parse(&obu).unwrap().is_some() {
                    count += 1;
                }
            }
            count
        })
    });
    group.finish();
}

fn bench_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("batch_decode");
    group.measurement_time(Duration::from_secs(10));
    for case in CASES {
        let Some(data) = vector(case.name) else {
            continue;
        };
        let samples = output_samples(&data, case.sound_system);
        group.throughput(Throughput::Elements(samples));
        group.bench_with_input(BenchmarkId::from_parameter(case.label), &data, |b, data| {
            b.iter(|| {
                let descriptors = Descriptors::collect(data).unwrap();
                let target = SoundSystem::from_u8(case.sound_system).unwrap();
                let mut decoder =
                    PresentationDecoder::new(&descriptors, 0, target, &DefaultFactory).unwrap();
                for obu in ObuIter::new(data).map(Result::unwrap) {
                    decoder.process_obu(&obu).unwrap();
                }
                decoder.finish().unwrap().interleaved.len()
            })
        });
    }
    group.finish();
}

fn bench_stream(c: &mut Criterion) {
    let mut group = c.benchmark_group("stream_decode");
    group.measurement_time(Duration::from_secs(10));
    for case in CASES {
        let Some(data) = vector(case.name) else {
            continue;
        };
        let samples = output_samples(&data, case.sound_system);
        group.throughput(Throughput::Elements(samples));
        group.bench_with_input(BenchmarkId::from_parameter(case.label), &data, |b, data| {
            b.iter(|| {
                let mut settings = StreamSettings::default();
                settings.layout = SoundSystem::from_u8(case.sound_system).unwrap();
                let mut decoder =
                    StreamDecoder::new_from_descriptors(data, settings, &DefaultFactory).unwrap();
                let mut bytes = 0usize;
                // Feed in demuxer-sized chunks and drain as units complete.
                for chunk in data.chunks(4096) {
                    decoder.decode(chunk).unwrap();
                    while decoder.is_temporal_unit_available() {
                        bytes += decoder.get_output_temporal_unit().unwrap().unwrap().len();
                    }
                }
                bytes
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_obu_walk, bench_batch, bench_stream);
criterion_main!(benches);
