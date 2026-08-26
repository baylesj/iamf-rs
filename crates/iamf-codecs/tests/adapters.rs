//! Codec adapter tests against real vector bitstreams: FLAC and AAC-LC
//! substream decoding shape, Opus version validation, and pure-Rust vs
//! libopus equivalence. Vectors come from tools/fetch_vectors.sh; missing
//! files fail unless IAMF_VECTORS_OPTIONAL=1 downgrades them to skips.

use std::path::PathBuf;

use iamf_dec::{CodecFactory, DecodedFrame};
use iamf_obu::descriptors::{self, CodecConfig, Descriptor};
use iamf_obu::{AudioFrame, ObuIter};

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

fn vector(name: &str) -> Option<Vec<u8>> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/vectors")
        .join(name);
    std::fs::read(path).ok()
}

/// The codec config and the first element's first-substream frames
/// (payload bytes) of a vector.
fn first_substream(data: &[u8]) -> (CodecConfig, u8, Vec<Vec<u8>>) {
    let mut codec_config = None;
    let mut substream_id = None;
    let mut channels = 0u8;
    let mut frames = Vec::new();
    for obu in ObuIter::new(data).map(Result::unwrap) {
        match descriptors::parse(&obu).unwrap() {
            Some(Descriptor::CodecConfig(cc)) if codec_config.is_none() => {
                codec_config = Some(cc);
            }
            Some(Descriptor::AudioElement(ae)) if substream_id.is_none() => {
                substream_id = ae.substream_ids.first().copied();
                channels = iamf_dec::element::substream_channels(&ae.config)[0];
            }
            _ => {}
        }
        if let Some(frame) = AudioFrame::from_obu(&obu).unwrap() {
            if Some(frame.substream_id) == substream_id {
                frames.push(frame.data.to_vec());
            }
        }
    }
    (codec_config.unwrap(), channels, frames)
}

fn decode_frames(
    factory: &dyn CodecFactory,
    config: &CodecConfig,
    channels: u8,
    frames: &[Vec<u8>],
) -> Vec<DecodedFrame> {
    let mut decoder = factory.create(config, channels).unwrap();
    frames
        .iter()
        .map(|packet| {
            let mut out = DecodedFrame::default();
            decoder.decode(packet, &mut out).unwrap();
            out
        })
        .collect()
}

/// FLAC substream (coupled stereo layer of the scalable vector): the
/// element-wide STREAMINFO is patched down to the substream width.
#[cfg(feature = "flac")]
#[test]
fn flac_substream_decodes() {
    let data = require_vectors!(vector("test_000073.iamf"), "test_000073");
    let (config, channels, frames) = first_substream(&data);
    assert_eq!(channels, 2);
    let decoded = decode_frames(&iamf_codecs::flac::FlacFactory, &config, channels, &frames);
    assert!(!decoded.is_empty());
    for frame in &decoded {
        assert_eq!(frame.channels, 2);
        assert_eq!(
            frame.samples.len(),
            config.num_samples_per_frame as usize * 2
        );
        assert!(frame.samples.iter().all(|s| s.is_finite()));
    }
    assert!(
        decoded.iter().any(|f| f.samples.iter().any(|&s| s != 0.0)),
        "silent output"
    );
}

/// AAC-LC substream, initialized from the AudioSpecificConfig.
#[cfg(feature = "aac")]
#[test]
fn aac_substream_decodes() {
    let data = require_vectors!(vector("test_000090.iamf"), "test_000090");
    let (config, channels, frames) = first_substream(&data);
    let decoded = decode_frames(&iamf_codecs::aac::AacFactory, &config, channels, &frames);
    assert!(!decoded.is_empty());
    for frame in &decoded {
        assert_eq!(frame.channels, channels);
        assert_eq!(
            frame.samples.len(),
            config.num_samples_per_frame as usize * usize::from(channels)
        );
        assert!(frame.samples.iter().all(|s| s.is_finite()));
    }
    assert!(
        decoded
            .iter()
            .any(|f| f.samples.iter().any(|&s| s.abs() > 0.001)),
        "silent output"
    );
}

/// The pure-Rust and libopus decoders must agree closely on real frames
/// (both are RFC 6716/8251 conformant; small numeric differences allowed).
#[cfg(all(feature = "opus", feature = "opus-ffi"))]
#[test]
fn opus_pure_rust_matches_libopus() {
    let data = require_vectors!(vector("test_000026.iamf"), "test_000026");
    let (config, channels, frames) = first_substream(&data);
    let pure = decode_frames(&iamf_codecs::opus::OpusFactory, &config, channels, &frames);
    let ffi = decode_frames(
        &iamf_codecs::opus_ffi::OpusFfiFactory,
        &config,
        channels,
        &frames,
    );
    assert_eq!(pure.len(), ffi.len());
    let mut signal = 0f64;
    let mut noise = 0f64;
    for (a, b) in pure.iter().zip(&ffi) {
        assert_eq!(a.samples.len(), b.samples.len());
        assert_eq!(a.sample_rate, b.sample_rate);
        for (&x, &y) in a.samples.iter().zip(&b.samples) {
            signal += f64::from(y) * f64::from(y);
            noise += f64::from(x - y) * f64::from(x - y);
        }
    }
    assert!(signal > 0.0, "silent reference");
    let snr_db = 10.0 * (signal / noise.max(1e-30)).log10();
    assert!(
        snr_db > 60.0,
        "pure-Rust vs libopus SNR only {snr_db:.1} dB"
    );
}

/// §3.6.1 requires the Opus decoder-config version to be 1 (readers
/// tolerate up to 15); version 16 (vector 000025) must be rejected.
#[cfg(feature = "opus")]
#[test]
fn opus_bad_version_rejected() {
    use iamf_obu::descriptors::{CodecId, DecoderConfig};
    let config = |version: u8| CodecConfig {
        codec_config_id: 0,
        codec_id: CodecId::Opus,
        num_samples_per_frame: 960,
        audio_roll_distance: -4,
        decoder_config: DecoderConfig::Opus {
            version,
            output_channel_count: 2,
            pre_skip: 312,
            input_sample_rate: 48000,
            output_gain: 0,
            mapping_family: 0,
        },
    };
    let factory = iamf_codecs::opus::OpusFactory;
    assert!(factory.supports(&config(1)));
    assert!(!factory.supports(&config(16)));
    assert!(factory.create(&config(16), 2).is_err());
}
