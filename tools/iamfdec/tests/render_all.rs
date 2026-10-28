//! Survey of every libiamf vector in tests/vectors against its reference
//! renders. Ignored by default; run with
//!
//! ```text
//! IAMF_RENDER_REPORT=/tmp/report.tsv \
//!   cargo test --release -p iamfdec --test render_all -- --ignored --nocapture
//! ```
//!
//! For each vector marked decodable, every
//! `<vector>_rendered_id_<mix>_sub_mix_<n>_layout_<k>.wav` reference is
//! decoded with that mix presentation and layout and compared sample by
//! sample. Vectors marked not decodable must fail somewhere in decoding.
//! The report has one row per case; the test prints counts per status and
//! does not fail on mismatches.

mod common;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::vectors_dir;
use iamf_codecs::DefaultFactory;
use iamf_dec::layout::SoundSystem;
use iamf_dec::post::quantize_s16;
use iamf_dec::presentation::{Descriptors, PresentationDecoder, RenderedMix};
use iamf_dec::stream::{StreamDecoder, StreamSettings};
use iamf_obu::ObuIter;
use iamf_obu::descriptors::Layout;

struct Wav {
    channels: usize,
    bits: u16,
    /// Samples scaled to [-1, 1).
    samples: Vec<f64>,
    /// Raw 16-bit samples when `bits == 16`.
    s16: Vec<i16>,
}

fn read_wav(path: &Path) -> Option<Wav> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 12 || &data[..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return None;
    }
    let mut pos = 12;
    let (mut channels, mut bits) = (0usize, 0u16);
    while pos + 8 <= data.len() {
        let id = &data[pos..pos + 4];
        let size = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().ok()?) as usize;
        let body = &data[pos + 8..(pos + 8 + size).min(data.len())];
        match id {
            b"fmt " => {
                channels = usize::from(u16::from_le_bytes(body[2..4].try_into().ok()?));
                bits = u16::from_le_bytes(body[14..16].try_into().ok()?);
            }
            b"data" => {
                let bytes = usize::from(bits / 8);
                if bytes == 0 {
                    return None;
                }
                let scale = f64::from(1u32 << (bits - 1));
                let mut samples = Vec::with_capacity(body.len() / bytes);
                let mut s16 = Vec::new();
                for chunk in body.chunks_exact(bytes) {
                    let mut v: i32 = 0;
                    for (i, b) in chunk.iter().enumerate() {
                        v |= i32::from(*b) << (8 * i);
                    }
                    let shift = 32 - u32::from(bits);
                    let v = (v << shift) >> shift;
                    samples.push(f64::from(v) / scale);
                    if bits == 16 {
                        s16.push(v as i16);
                    }
                }
                return Some(Wav {
                    channels,
                    bits,
                    samples,
                    s16,
                });
            }
            _ => {}
        }
        pos += 8 + size + (size & 1);
    }
    None
}

#[derive(Clone)]
struct Case {
    vector: String,
    kind: CaseKind,
}

#[derive(Clone)]
enum CaseKind {
    Render {
        mix_id: u32,
        sub_mix: usize,
        layout_index: usize,
        reference: PathBuf,
    },
    MustFail,
    NoReference,
}

struct Row {
    vector: String,
    case: String,
    layout: String,
    status: &'static str,
    detail: String,
    max_diff_lsb16: f64,
    snr_db: f64,
    stream_decoder: String,
}

fn textproto_flags(path: &Path) -> (bool, bool) {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    (
        !text.contains("is_valid: false"),
        text.contains("is_valid_to_decode: true"),
    )
}

fn parse_reference_name(vector: &str, file: &str) -> Option<(u32, usize, usize)> {
    let rest = file.strip_prefix(vector)?.strip_prefix("_rendered_id_")?;
    let rest = rest.strip_suffix(".wav")?;
    let (mix, rest) = rest.split_once("_sub_mix_")?;
    let (sub, layout) = rest.split_once("_layout_")?;
    Some((mix.parse().ok()?, sub.parse().ok()?, layout.parse().ok()?))
}

fn decode(
    data: &[u8],
    descriptors: &Descriptors,
    mix_index: usize,
    target: SoundSystem,
) -> Result<RenderedMix, String> {
    let mut decoder = PresentationDecoder::new(descriptors, mix_index, target, &DefaultFactory)
        .map_err(|e| format!("create: {e}"))?;
    for obu in ObuIter::new(data) {
        let obu = obu.map_err(|e| format!("framing: {e}"))?;
        decoder
            .process_obu(&obu)
            .map_err(|e| format!("{:?}: {e}", obu.header.obu_type))?;
    }
    decoder.finish().map_err(|e| format!("finish: {e}"))
}

fn stream_decoder_status(data: &[u8]) -> String {
    match StreamDecoder::new_from_descriptors(data, StreamSettings::default(), &DefaultFactory) {
        Ok(_) => "ok".into(),
        Err(e) => format!("err: {e}"),
    }
}

fn run_case(case: &Case) -> Row {
    let dir = vectors_dir();
    let mut row = Row {
        vector: case.vector.clone(),
        case: String::new(),
        layout: String::new(),
        status: "",
        detail: String::new(),
        max_diff_lsb16: f64::NAN,
        snr_db: f64::NAN,
        stream_decoder: String::new(),
    };
    let data = match std::fs::read(dir.join(format!("{}.iamf", case.vector))) {
        Ok(d) => d,
        Err(e) => {
            row.status = "missing_file";
            row.detail = e.to_string();
            return row;
        }
    };
    row.stream_decoder = stream_decoder_status(&data);
    let descriptors = Descriptors::collect(&data);

    match &case.kind {
        CaseKind::MustFail => {
            row.case = "must_fail".into();
            let result = descriptors.map_err(|e| format!("collect: {e}")).and_then(|d| {
                if d.mix_presentations.is_empty() {
                    return Err("no mix presentations".into());
                }
                decode(&data, &d, 0, SoundSystem::A)
            });
            match result {
                Err(e) => {
                    row.status = "rejected";
                    row.detail = e;
                }
                Ok(_) => row.status = "accepted_invalid",
            }
        }
        CaseKind::NoReference => {
            row.case = "decode_only".into();
            let result = descriptors
                .map_err(|e| format!("collect: {e}"))
                .and_then(|d| decode(&data, &d, 0, SoundSystem::A));
            match result {
                Ok(_) => row.status = "decoded_no_reference",
                Err(e) => {
                    row.status = "decode_error";
                    row.detail = e;
                }
            }
        }
        CaseKind::Render {
            mix_id,
            sub_mix,
            layout_index,
            reference,
        } => {
            row.case = format!("id_{mix_id}_sub_mix_{sub_mix}_layout_{layout_index}");
            let descriptors = match descriptors {
                Ok(d) => d,
                Err(e) => {
                    row.status = "decode_error";
                    row.detail = format!("collect: {e}");
                    return row;
                }
            };
            let Some(mix_index) = descriptors
                .mix_presentations
                .iter()
                .position(|m| m.mix_presentation_id == *mix_id)
            else {
                row.status = "decode_error";
                row.detail = format!("mix presentation {mix_id} not found");
                return row;
            };
            let mix = &descriptors.mix_presentations[mix_index];
            let Some(sub) = mix.sub_mixes.get(*sub_mix) else {
                row.status = "unsupported";
                row.detail = format!("sub mix {sub_mix} of {}", mix.sub_mixes.len());
                return row;
            };
            let Some((layout, _)) = sub.layouts.get(*layout_index) else {
                row.status = "decode_error";
                row.detail = format!("layout {layout_index} of {}", sub.layouts.len());
                return row;
            };
            let target = match layout {
                Layout::LoudspeakersSsConvention { sound_system } => {
                    row.layout = format!("SS{sound_system}");
                    SoundSystem::from_u8(*sound_system)
                }
                Layout::Binaural => {
                    row.layout = "binaural".into();
                    Some(SoundSystem::Binaural)
                }
                Layout::Reserved { layout_type } => {
                    row.layout = format!("reserved{layout_type}");
                    None
                }
            };
            let Some(target) = target else {
                row.status = "unsupported";
                row.detail = format!("layout {}", row.layout);
                return row;
            };
            if *sub_mix != 0 {
                row.status = "unsupported";
                row.detail = "sub mix other than 0".into();
                return row;
            }
            let Some(wav) = read_wav(reference) else {
                row.status = "bad_reference";
                return row;
            };
            let ours = match decode(&data, &descriptors, mix_index, target) {
                Ok(m) => m,
                Err(e) => {
                    row.status = "decode_error";
                    row.detail = e;
                    return row;
                }
            };
            if ours.channels != wav.channels {
                row.status = "channel_mismatch";
                row.detail = format!("ours {} ref {}", ours.channels, wav.channels);
                return row;
            }
            if ours.interleaved.len() != wav.samples.len() {
                row.status = "length_mismatch";
                row.detail = format!(
                    "ours {} ref {} frames",
                    ours.interleaved.len() / ours.channels.max(1),
                    wav.samples.len() / wav.channels.max(1)
                );
            }
            let n = ours.interleaved.len().min(wav.samples.len());
            let (mut max_diff, mut sig, mut err) = (0.0f64, 0.0f64, 0.0f64);
            for i in 0..n {
                let d = if wav.bits == 16 {
                    f64::from(
                        (i32::from(quantize_s16(ours.interleaved[i])) - i32::from(wav.s16[i]))
                            .abs(),
                    )
                } else {
                    let a = f64::from(ours.interleaved[i]).clamp(-1.0, 1.0);
                    (a - wav.samples[i]).abs() * 32768.0
                };
                max_diff = max_diff.max(d);
                sig += wav.samples[i] * wav.samples[i];
                err += (d / 32768.0) * (d / 32768.0);
            }
            row.max_diff_lsb16 = max_diff;
            row.snr_db = if err == 0.0 {
                f64::INFINITY
            } else {
                10.0 * (sig / err).log10()
            };
            if row.status.is_empty() {
                row.status = if max_diff == 0.0 {
                    "exact"
                } else if max_diff <= 1.0 {
                    "within_1_lsb"
                } else if max_diff <= 32.0 {
                    "within_32_lsb"
                } else {
                    "mismatch"
                };
            }
        }
    }
    row
}

fn collect_cases() -> Vec<Case> {
    let dir = vectors_dir();
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .expect("tests/vectors missing; run tools/fetch_vectors.sh --all")
        .filter_map(Result::ok)
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    let mut cases = Vec::new();
    for name in names.iter().filter(|n| n.ends_with(".iamf")) {
        let vector = name.trim_end_matches(".iamf").to_string();
        let (_, decodable) = textproto_flags(&dir.join(format!("{vector}.textproto")));
        if !decodable {
            cases.push(Case {
                vector,
                kind: CaseKind::MustFail,
            });
            continue;
        }
        let prefix = format!("{vector}_rendered_id_");
        let mut found = false;
        for file in names.iter().filter(|n| n.starts_with(&prefix)) {
            if let Some((mix_id, sub_mix, layout_index)) = parse_reference_name(&vector, file) {
                found = true;
                cases.push(Case {
                    vector: vector.clone(),
                    kind: CaseKind::Render {
                        mix_id,
                        sub_mix,
                        layout_index,
                        reference: dir.join(file),
                    },
                });
            }
        }
        if !found {
            cases.push(Case {
                vector,
                kind: CaseKind::NoReference,
            });
        }
    }
    cases
}

#[test]
#[ignore = "survey over all vectors; slow, writes a report"]
fn render_all_vectors() {
    let cases = collect_cases();
    let next = AtomicUsize::new(0);
    let rows = Mutex::new(Vec::new());
    let threads = std::thread::available_parallelism().map_or(8, |n| n.get().min(32));
    std::thread::scope(|scope| {
        for _ in 0..threads {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(case) = cases.get(i) else { break };
                    let row = catch_unwind(AssertUnwindSafe(|| run_case(case))).unwrap_or_else(
                        |payload| {
                            let msg = payload
                                .downcast_ref::<String>()
                                .cloned()
                                .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
                                .unwrap_or_default();
                            Row {
                                vector: case.vector.clone(),
                                case: String::new(),
                                layout: String::new(),
                                status: "panic",
                                detail: msg,
                                max_diff_lsb16: f64::NAN,
                                snr_db: f64::NAN,
                                stream_decoder: String::new(),
                            }
                        },
                    );
                    rows.lock().unwrap().push((i, row));
                }
            });
        }
    });
    let mut rows = rows.into_inner().unwrap();
    rows.sort_by_key(|(i, _)| *i);

    let mut report = String::from(
        "vector\tcase\tlayout\tstatus\tmax_diff_lsb16\tsnr_db\tstream_decoder\tdetail\n",
    );
    let mut counts = std::collections::BTreeMap::<&str, usize>::new();
    for (_, r) in &rows {
        *counts.entry(r.status).or_default() += 1;
        report.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{:.1}\t{}\t{}\n",
            r.vector,
            r.case,
            r.layout,
            r.status,
            r.max_diff_lsb16,
            r.snr_db,
            r.stream_decoder,
            r.detail.replace(['\t', '\n'], " ")
        ));
    }
    if let Some(path) = std::env::var_os("IAMF_RENDER_REPORT") {
        std::fs::write(&path, &report).expect("write report");
        println!("report: {}", PathBuf::from(path).display());
    }
    println!("{} cases", rows.len());
    for (status, n) in counts {
        println!("{status:>22}: {n}");
    }
}
