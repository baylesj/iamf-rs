//! ROAR vs builtin renderer equivalence on the curated conformance vectors
//! (`roar` feature). The backends share everything upstream of rendering
//! (decode, trimming, demixing, layer selection, parameter timing), so the
//! comparison isolates the renderers: libiamf's gain matrices vs OAR's EAR
//! / IAMF-downmix renderers. They are not bit-exact by design; see
//! `ARCHITECTURE.md` ("Renderer backends") for the measured numbers and the
//! explanation of each divergent case.

#![cfg(feature = "roar")]

mod common;

use iamf_codecs::DefaultFactory;
use iamf_dec::layout::SoundSystem;
use iamf_dec::renderer::RendererBackend;
use iamf_dec::stream::{MixSelection, OutputSampleType, StreamDecoder, StreamSettings};

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
];

fn vector(name: &str) -> Option<Vec<u8>> {
    std::fs::read(common::vectors_dir().join(format!("{name}.iamf"))).ok()
}

fn decode(data: &[u8], target: SoundSystem, renderer: RendererBackend) -> Option<Vec<f64>> {
    let mut settings = StreamSettings::default();
    settings.layout = target;
    settings.sample_type = Some(OutputSampleType::Int32LittleEndian);
    settings.mix_selection = MixSelection::ByIndex(0);
    settings.renderer = renderer;
    let mut decoder = StreamDecoder::new_from_descriptors(data, settings, &DefaultFactory).ok()?;
    decoder.decode(data).ok()?;
    let mut out = Vec::new();
    while let Some(bytes) = decoder.get_output_temporal_unit().ok()? {
        out.extend(
            bytes
                .chunks_exact(4)
                .map(|b| f64::from(i32::from_le_bytes(b.try_into().unwrap())) / 2_147_483_648.0),
        );
    }
    Some(out)
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

/// Scene-based elements rendered to sound system H (22.2, LFEs at output
/// channels 3 and 9): ROAR's ambisonics→channel renderer reinserts the
/// LFE slots by comparing LFE positions against *source* indices, so the
/// non-LFE channel feeding output 10 (SiL) is written into LFE2 and then
/// zeroed, and output 10 keeps the matrix's source-10 channel (TpFL,
/// duplicated at output 12). An upstream ROAR bug
/// (`ambisonic_to_channel_renderer.rs`, LFE map); all other channels match.
const H_OUTPUT_SILEFT: usize = 10;

fn is_scene_based_only(name: &str) -> bool {
    matches!(
        name,
        "test_000038"
            | "test_000039"
            | "test_000042"
            | "test_000048"
            | "test_000065"
            | "test_000066"
    )
}

fn has_scene_based(name: &str) -> bool {
    is_scene_based_only(name) || name == "test_000086"
}

#[test]
fn roar_matches_builtin_on_loudspeakers() {
    for name in VECTORS {
        let data = require_vectors!(vector(name), format_args!("{name}"));
        for ss in 0..=13u8 {
            let target = SoundSystem::from_u8(ss).unwrap();
            let channels = target.channels();
            let Some(builtin) = decode(&data, target, RendererBackend::Builtin) else {
                continue;
            };
            let roar = decode(&data, target, RendererBackend::Roar)
                .unwrap_or_else(|| panic!("{name} SS{ss}: ROAR failed where builtin decoded"));
            let (snr, max_diff) = compare(&builtin, &roar, channels, |_| true);
            eprintln!("{name} SS{ss:2}: SNR {snr:7.1} dB, max diff {max_diff:.2e}");

            if DOWNMIX_CASES.contains(&(*name, ss)) {
                assert!(
                    snr >= DOWNMIX_MIN_SNR_DB,
                    "{name} SS{ss}: OAR down-mix diverged beyond expectation ({snr:.1} dB)"
                );
            } else if target == SoundSystem::H && has_scene_based(name) {
                let (snr, max_diff) = compare(&builtin, &roar, channels, |c| c != H_OUTPUT_SILEFT);
                assert!(
                    snr >= MATCH_SNR_DB && max_diff <= MATCH_MAX_DIFF,
                    "{name} SS7 minus ch{H_OUTPUT_SILEFT}: {snr:.1} dB, {max_diff:.2e}"
                );
                if is_scene_based_only(name) {
                    // Pin the bug's exact shape: ROAR's output 10 carries
                    // what builtin (correctly) renders to output 12.
                    let frames = builtin.len() / channels;
                    let shifted_b: Vec<f64> = (0..frames)
                        .map(|t| builtin[t * channels + H_OUTPUT_SILEFT + 2])
                        .collect();
                    let roar_10: Vec<f64> = (0..frames)
                        .map(|t| roar[t * channels + H_OUTPUT_SILEFT])
                        .collect();
                    let (snr, _) = compare(&shifted_b, &roar_10, 1, |_| true);
                    assert!(snr >= MATCH_SNR_DB, "{name}: ROAR SS H ch10 shape changed");
                }
            } else {
                assert!(
                    snr >= MATCH_SNR_DB && max_diff <= MATCH_MAX_DIFF,
                    "{name} SS{ss}: ROAR diverged from builtin ({snr:.1} dB, {max_diff:.2e})"
                );
            }
        }
    }
}

/// Binaural: both backends are ports of google/obr, but ROAR routes
/// scene-based elements through OBR regardless of headphones_rendering_mode
/// and uses its own HRIR set selection (`BinauralFilterProfile::Ambient`),
/// so only a coarse agreement is expected. Requires sane, non-silent,
/// correctly-sized output and reports the SNR.
#[test]
fn roar_binaural_smoke() {
    for (name, hrtf) in [
        ("test_000038", false),
        ("test_000070", false),
        ("test_000038", true),
        ("test_000070", true),
    ] {
        let mut data = require_vectors!(vector(name), format_args!("{name}"));
        if hrtf {
            common::set_binaural_mode(&mut data);
        }
        let builtin = decode(&data, SoundSystem::Binaural, RendererBackend::Builtin).unwrap();
        let roar = decode(&data, SoundSystem::Binaural, RendererBackend::Roar).unwrap();
        assert_eq!(builtin.len(), roar.len(), "{name}: length mismatch");
        let energy: f64 = roar.iter().map(|s| s * s).sum();
        assert!(energy > 0.0, "{name}: ROAR binaural output is silent");
        assert!(roar.iter().all(|s| s.is_finite() && s.abs() <= 1.0));
        let (snr, max_diff) = compare(&builtin, &roar, 2, |_| true);
        eprintln!(
            "{name} binaural (mode {}): SNR {snr:7.1} dB, max diff {max_diff:.2e}",
            u8::from(hrtf)
        );
    }
}

/// Selecting ROAR goes through the same configuration checks and yields
/// the same output geometry (channels, rate, frame size).
#[test]
fn roar_decoder_geometry() {
    let data = require_vectors!(vector("test_000070"), format_args!("test_000070"));
    for renderer in [RendererBackend::Builtin, RendererBackend::Roar] {
        let mut settings = StreamSettings::default();
        settings.layout = SoundSystem::J;
        settings.renderer = renderer;
        let decoder =
            StreamDecoder::new_from_descriptors(&data, settings, &DefaultFactory).unwrap();
        assert_eq!(decoder.num_output_channels(), 12);
        assert_eq!(decoder.sample_rate(), 48000);
    }
}
