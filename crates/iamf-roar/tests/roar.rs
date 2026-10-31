//! ROAR vs builtin renderer equivalence on the curated conformance vectors.
//! Both paths share everything upstream of rendering (decode, trimming,
//! demixing, parameter timing) in iamf-dec's streaming decoder, so the
//! comparison isolates the renderers: libiamf's gain matrices vs OAR's EAR
//! / IAMF-downmix renderers. They are not bit-exact by design; see
//! `ARCHITECTURE.md` ("Renderer backends") for the measured numbers and the
//! explanation of each divergent case.

mod common;

use std::sync::Arc;

use iamf_codecs::DefaultFactory;
use iamf_dec::DecodeError;
use iamf_dec::layout::SoundSystem;
use iamf_dec::profile::ProfileSet;
use iamf_dec::stream::{MixSelection, OutputSampleType, StreamDecoder, StreamSettings};
use iamf_roar::RoarRendererFactory;

/// Which renderer a test decode uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Renderer {
    Builtin,
    Roar,
}

/// Valid curated vectors (000007/000025 are should-fail cases).
const VECTORS: &[&str] = &[
    "test_000002",
    "test_000005",
    "test_000024",
    "test_000026",
    "test_000032",
    "test_000033",
    "test_000036",
    "test_000038",
    "test_000039",
    "test_000042",
    "test_000048",
    "test_000065",
    "test_000066",
    "test_000069",
    "test_000070",
    "test_000073",
    "test_000082",
    "test_000086",
    "test_000088",
    "test_000090",
    "test_000092",
    "test_000611",
    "test_000828",
    "test_000829",
    "test_000830",
];

fn vector(name: &str) -> Option<Vec<u8>> {
    std::fs::read(common::vectors_dir().join(format!("{name}.iamf"))).ok()
}

fn new_decoder(
    data: &[u8],
    settings: StreamSettings,
    renderer: Renderer,
) -> Result<StreamDecoder, DecodeError> {
    match renderer {
        Renderer::Builtin => StreamDecoder::new_from_descriptors(data, settings, &DefaultFactory),
        Renderer::Roar => StreamDecoder::new_from_descriptors_with_renderer(
            data,
            settings,
            &DefaultFactory,
            Arc::new(RoarRendererFactory::new()),
        ),
    }
}

fn try_decode(
    data: &[u8],
    target: SoundSystem,
    renderer: Renderer,
) -> Result<Vec<f64>, DecodeError> {
    let mut settings = StreamSettings::default();
    settings.layout = target;
    settings.sample_type = Some(OutputSampleType::Int32LittleEndian);
    settings.mix_selection = MixSelection::ByIndex(0);
    // headphones_rendering_mode 2 (head-locked) is allowed only by the IAMF
    // v2.0 profiles, which the default request leaves out.
    settings.requested_profiles = ProfileSet::all();
    let mut decoder = new_decoder(data, settings, renderer)?;
    decoder.decode(data)?;
    let mut out = Vec::new();
    while let Some(bytes) = decoder.get_output_temporal_unit()? {
        out.extend(
            bytes
                .as_chunks::<4>()
                .0
                .iter()
                .map(|&b| f64::from(i32::from_le_bytes(b)) / 2_147_483_648.0),
        );
    }
    Ok(out)
}

fn decode(data: &[u8], target: SoundSystem, renderer: Renderer) -> Option<Vec<f64>> {
    try_decode(data, target, renderer).ok()
}

/// (SNR in dB of ROAR relative to builtin, max absolute sample difference
/// in full-scale units), over interleaved samples of `channels` channels,
/// restricted to the channels `keep` accepts.
fn compare(
    builtin: &[f64],
    roar: &[f64],
    channels: usize,
    keep: impl Fn(usize) -> bool,
) -> (f64, f64) {
    assert_eq!(builtin.len(), roar.len(), "output lengths differ");
    let (mut signal, mut noise, mut max_diff) = (0.0f64, 0.0f64, 0.0f64);
    for (i, (&b, &r)) in builtin.iter().zip(roar).enumerate() {
        if !keep(i % channels) {
            continue;
        }
        signal += b * b;
        noise += (b - r) * (b - r);
        max_diff = max_diff.max((b - r).abs());
    }
    let snr = if noise == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (signal / noise).log10()
    };
    (snr, max_diff)
}

/// Where both backends run the same libiamf/EAR gain tables, they agree to
/// float rounding: measured ≥ 141 dB SNR and ≤ 1.2e-7 max difference
/// (summation order differs), most cases bit-exact. The thresholds leave
/// ~20 dB / 8x headroom for other platforms' FMA/vectorization.
const MATCH_SNR_DB: f64 = 120.0;
const MATCH_MAX_DIFF: f64 = 1e-6;

/// Channel-based 7.1.4 elements carrying demixing info, rendered to a
/// layout that is a valid IAMF down-mix of 7.1.4 *with* height channels
/// (5.1.2, 5.1.4, 7.1.2, 3.1.2): OAR selects its IAMF down-mix renderer
/// (§7.2 dmixp_mode matrices: Ls5 = α·Lss7 + β·Lrs7, Ltf2 = Ltf + γ·Ltb,
/// …) where libiamf/iamf-tools render with EAR-derived matrices. Expected
/// design difference, not a bug. Measured SNR 6.6–88 dB (000082 varies
/// its demixing mode per frame; 000070 uses the default mode throughout).
const DOWNMIX_CASES: &[(&str, u8)] = &[
    ("test_000070", 2),
    ("test_000070", 3),
    ("test_000070", 10),
    ("test_000070", 11),
    ("test_000082", 2),
    ("test_000082", 3),
    ("test_000082", 10),
    ("test_000082", 11),
];
/// Sanity floor for the down-mix cases: same content, different matrix.
const DOWNMIX_MIN_SNR_DB: f64 = 5.0;

/// Vectors with scene-based elements. ROAR renders those to sound system
/// H (22.2, LFEs at output channels 3 and 9) incorrectly at the pinned
/// revision: its ambisonics to channel renderer re-inserts the LFE slots
/// by comparing LFE positions against *source* indices, so the channel
/// that belongs in output 10 (SiL) is written into LFE2 and then zeroed,
/// and output 10 keeps a copy of output 12 (TpFL). `RoarRendererFactory`
/// refuses that combination until ROAR is fixed.
fn has_scene_based(name: &str) -> bool {
    matches!(
        name,
        "test_000038"
            | "test_000039"
            | "test_000042"
            | "test_000048"
            | "test_000065"
            | "test_000066"
            | "test_000086"
            | "test_000611"
    )
}

#[test]
fn roar_matches_builtin_on_loudspeakers() {
    for name in VECTORS {
        let data = require_vectors!(vector(name), format_args!("{name}"));
        for ss in 0..=14u8 {
            let target = SoundSystem::from_bitstream_sound_system(ss).unwrap();
            let channels = target.channels();
            let Some(builtin) = decode(&data, target, Renderer::Builtin) else {
                continue;
            };
            if target == SoundSystem::H && has_scene_based(name) {
                // Refused (upstream ROAR 22.2 LFE bug, see has_scene_based).
                let result = try_decode(&data, target, Renderer::Roar);
                assert!(
                    matches!(result, Err(DecodeError::UnsupportedRenderer(_))),
                    "{name} SS{ss}: expected the ROAR 22.2 refusal, got {result:?}"
                );
                continue;
            }
            let roar = decode(&data, target, Renderer::Roar)
                .unwrap_or_else(|| panic!("{name} SS{ss}: ROAR failed where builtin decoded"));
            let (snr, max_diff) = compare(&builtin, &roar, channels, |_| true);
            eprintln!("{name} SS{ss:2}: SNR {snr:7.1} dB, max diff {max_diff:.2e}");

            if DOWNMIX_CASES.contains(&(*name, ss)) {
                assert!(
                    snr >= DOWNMIX_MIN_SNR_DB,
                    "{name} SS{ss}: OAR down-mix diverged beyond expectation ({snr:.1} dB)"
                );
            } else {
                assert!(
                    snr >= MATCH_SNR_DB && max_diff <= MATCH_MAX_DIFF,
                    "{name} SS{ss}: ROAR diverged from builtin ({snr:.1} dB, {max_diff:.2e})"
                );
            }
        }
    }
}

/// Binaural: both renderers are ports of google/obr, but ROAR routes
/// scene-based elements through OBR regardless of headphones_rendering_mode
/// and uses its own HRIR set selection (`BinauralFilterProfile::Ambient`),
/// so only a coarse agreement is expected. Requires sane, non-silent,
/// correctly-sized output and reports the SNR. Covers modes 0 (stereo),
/// 1 (world-locked) and 2 (head-locked); for 1 and 2 the decoder hands
/// ROAR the highest layer (7.1.4 for test_000070).
#[test]
fn roar_binaural_smoke() {
    for name in ["test_000038", "test_000070"] {
        for mode in 0..=2u8 {
            let mut data = require_vectors!(vector(name), format_args!("{name}"));
            common::set_headphones_rendering_mode(&mut data, mode);
            let builtin = decode(&data, SoundSystem::Binaural, Renderer::Builtin).unwrap();
            let roar = decode(&data, SoundSystem::Binaural, Renderer::Roar).unwrap();
            assert_eq!(builtin.len(), roar.len(), "{name}: length mismatch");
            let energy: f64 = roar.iter().map(|s| s * s).sum();
            assert!(energy > 0.0, "{name}: ROAR binaural output is silent");
            assert!(roar.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
            let (snr, max_diff) = compare(&builtin, &roar, 2, |_| true);
            eprintln!("{name} binaural (mode {mode}): SNR {snr:7.1} dB, max diff {max_diff:.2e}");
        }
    }
}

/// Rendering through ROAR goes through the same configuration checks and
/// yields the same output geometry (channels, rate, frame size).
#[test]
fn roar_decoder_geometry() {
    let data = require_vectors!(vector("test_000070"), format_args!("test_000070"));
    for renderer in [Renderer::Builtin, Renderer::Roar] {
        let mut settings = StreamSettings::default();
        settings.layout = SoundSystem::J;
        let decoder = new_decoder(&data, settings, renderer).unwrap();
        assert_eq!(decoder.num_output_channels(), 12);
        assert_eq!(decoder.sample_rate(), 48000);
    }
}

/// `iamfdec-roar` writes exactly what the streaming decoder produces with
/// the same loudness and limiter settings: both run inside the decoder,
/// before quantization, with no second pass on the quantized samples.
#[test]
fn cli_matches_streaming_decoder() {
    let name = "test_000070";
    let path = common::vectors_dir().join(format!("{name}.iamf"));
    let data = require_vectors!(vector(name), format_args!("{name}"));
    let out = std::env::temp_dir().join(format!("iamf-roar-cli-{}.wav", std::process::id()));
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_iamfdec-roar"))
        .arg(&path)
        .args(["-s", "1", "--loudness", "0", "--limiter", "-o"])
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success());
    let wav = std::fs::read(&out).unwrap();
    std::fs::remove_file(&out).ok();
    assert_eq!(&wav[..4], b"RIFF");
    assert_eq!(
        u16::from_le_bytes([wav[22], wav[23]]),
        6,
        "5.1 channel count"
    );

    let mut settings = StreamSettings::default();
    settings.layout = SoundSystem::B;
    settings.sample_type = Some(OutputSampleType::Int16LittleEndian);
    settings.loudness_target_db = Some(0.0);
    settings.enable_limiter = true;
    let mut decoder = new_decoder(&data, settings, Renderer::Roar).unwrap();
    decoder.decode(&data).unwrap();
    // With the limiter on, the last unit waits for the end of the stream.
    decoder.signal_end_of_decoding();
    let mut pcm = Vec::new();
    while let Some(bytes) = decoder.get_output_temporal_unit().unwrap() {
        pcm.extend(bytes);
    }
    assert_ne!(pcm.len(), 0);
    assert_eq!(&wav[44..], pcm.as_slice());
    // The limiter (-1 dBFS) keeps every sample off full scale.
    let peak = pcm
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&b| i16::from_le_bytes(b).unsigned_abs())
        .max()
        .unwrap();
    assert!(peak < i16::MAX.unsigned_abs(), "peak {peak} hit full scale");
}
