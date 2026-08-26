//! Inspects a standalone .iamf bitstream: OBU listing plus parsed
//! descriptor summaries.
//!
//! Will grow into a full decode-to-WAV tool (the Rust counterpart of
//! libiamf's iamfdec) as pipeline milestones land.

use std::process::ExitCode;

use iamf_codecs::DefaultFactory;
use iamf_dec::layout::SoundSystem;
use iamf_dec::presentation::{Descriptors, PresentationDecoder};
use iamf_obu::ObuIter;
use iamf_obu::descriptors::{self, AudioElementConfig, Descriptor, Layout};

struct Options {
    sound_system: u8,
    limiter: bool,
    /// Target loudness in dB for normalization, when set.
    loudness: Option<f32>,
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1).collect::<Vec<_>>();
    let mut opts = Options {
        sound_system: 0,
        limiter: false,
        loudness: None,
    };
    let mut wav_out = None;
    let mut path = None;
    while !args.is_empty() {
        match args.remove(0).as_str() {
            "-o" if !args.is_empty() => wav_out = Some(args.remove(0)),
            "-s" if !args.is_empty() => match args.remove(0).parse() {
                Ok(s) => opts.sound_system = s,
                Err(_) => {
                    eprintln!("error: -s expects a sound system number (0..=13)");
                    return ExitCode::FAILURE;
                }
            },
            "--limiter" => opts.limiter = true,
            "--loudness" if !args.is_empty() => match args.remove(0).parse() {
                Ok(db) => opts.loudness = Some(db),
                Err(_) => {
                    eprintln!("error: --loudness expects a dB value (e.g. -24)");
                    return ExitCode::FAILURE;
                }
            },
            arg if path.is_none() => path = Some(arg.to_string()),
            _ => {
                eprintln!(
                    "usage: iamfdec <file.iamf> [-o out.wav] [-s sound_system] [--limiter] [--loudness dB]"
                );
                return ExitCode::FAILURE;
            }
        }
    }
    let Some(path) = path else {
        eprintln!(
            "usage: iamfdec <file.iamf> [-o out.wav] [-s sound_system] [--limiter] [--loudness dB]"
        );
        return ExitCode::FAILURE;
    };
    let data = match std::fs::read(&path) {
        Ok(data) => data,
        Err(err) => {
            eprintln!("error: cannot read {path}: {err}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(out) = wav_out {
        return decode_to_wav(&data, &out, &opts);
    }

    let mut counts = (0usize, 0usize); // (descriptor OBUs, audio frame OBUs)
    for result in ObuIter::new(&data) {
        let obu = match result {
            Ok(obu) => obu,
            Err(err) => {
                eprintln!("error: {err}");
                return ExitCode::FAILURE;
            }
        };
        if obu.header.obu_type.is_audio_frame() {
            counts.1 += 1;
            continue;
        }
        match descriptors::parse(&obu) {
            Ok(Some(descriptor)) => {
                counts.0 += 1;
                describe(&descriptor);
            }
            Ok(None) => {}
            Err(err) => {
                eprintln!("error: {:?}: {err}", obu.header.obu_type);
                return ExitCode::FAILURE;
            }
        }
    }
    println!(
        "{} descriptor OBUs, {} audio frame OBUs",
        counts.0, counts.1
    );
    ExitCode::SUCCESS
}

/// Decodes the first mix presentation and renders it to the target sound
/// system, writing 16-bit WAV.
fn decode_to_wav(data: &[u8], out_path: &str, opts: &Options) -> ExitCode {
    let sound_system = opts.sound_system;
    let Some(target) = SoundSystem::from_u8(sound_system) else {
        eprintln!("error: unknown sound system {sound_system}");
        return ExitCode::FAILURE;
    };
    let descriptors = match Descriptors::collect(data) {
        Ok(d) => d,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };
    let mut decoder = match PresentationDecoder::new(&descriptors, 0, target, &DefaultFactory) {
        Ok(d) => d,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };

    let mut frames = 0usize;
    for result in ObuIter::new(data) {
        let obu = match result {
            Ok(obu) => obu,
            Err(err) => {
                eprintln!("error: {err}");
                return ExitCode::FAILURE;
            }
        };
        match decoder.process_obu(&obu) {
            Ok(consumed) => frames += usize::from(consumed && obu.header.obu_type.is_audio_frame()),
            Err(err) => {
                eprintln!("error: {:?}: {err}", obu.header.obu_type);
                return ExitCode::FAILURE;
            }
        }
    }

    let mut mix = match decoder.finish() {
        Ok(mix) => mix,
        Err(err) => {
            eprintln!("error: {err}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(target_db) = opts.loudness {
        // Content loudness: the mix presentation's integrated loudness for
        // the rendered layout (Q7.8 dB), 0 dB when not declared.
        let content_db = descriptors
            .mix_presentations
            .first()
            .and_then(|mp| mp.sub_mixes.first())
            .and_then(|sm| {
                sm.layouts
                    .iter()
                    .find_map(|(layout, loudness)| match layout {
                        iamf_obu::descriptors::Layout::LoudspeakersSsConvention {
                            sound_system: s,
                        } if *s == sound_system => {
                            Some(f32::from(loudness.integrated_loudness) / 256.0)
                        }
                        _ => None,
                    })
            })
            .unwrap_or(0.0);
        iamf_dec::post::normalize_loudness(&mut mix.interleaved, target_db, content_db);
    }
    if opts.limiter {
        let mut limiter = iamf_dec::post::PeakLimiter::new(
            iamf_dec::post::LIMITER_THRESHOLD_DB,
            mix.sample_rate,
            mix.channels,
            iamf_dec::post::LIMITER_LOOKAHEAD,
        );
        mix.interleaved = limiter.process(&mix.interleaved);
    }
    let frame_count = mix.interleaved.len() / mix.channels.max(1);
    if let Err(err) = write_wav_s16(
        out_path,
        &mix.interleaved,
        mix.channels as u16,
        mix.sample_rate,
    ) {
        eprintln!("error: writing {out_path}: {err}");
        return ExitCode::FAILURE;
    }
    println!(
        "rendered {frames} frames -> {frame_count} samples x {} ch @ {} Hz (sound system {sound_system}) -> {out_path}",
        mix.channels, mix.sample_rate
    );
    ExitCode::SUCCESS
}

fn write_wav_s16(
    path: &str,
    samples: &[f32],
    channels: u16,
    sample_rate: u32,
) -> std::io::Result<()> {
    let data_len = (samples.len() * 2) as u32;
    let byte_rate = sample_rate * u32::from(channels) * 2;
    let mut wav = Vec::with_capacity(44 + samples.len() * 2);
    wav.extend(b"RIFF");
    wav.extend((36 + data_len).to_le_bytes());
    wav.extend(b"WAVEfmt ");
    wav.extend(16u32.to_le_bytes());
    wav.extend(1u16.to_le_bytes()); // PCM
    wav.extend(channels.to_le_bytes());
    wav.extend(sample_rate.to_le_bytes());
    wav.extend(byte_rate.to_le_bytes());
    wav.extend((channels * 2).to_le_bytes()); // block align
    wav.extend(16u16.to_le_bytes()); // bits per sample
    wav.extend(b"data");
    wav.extend(data_len.to_le_bytes());
    for &sample in samples {
        wav.extend(iamf_dec::post::quantize_s16(sample).to_le_bytes());
    }
    std::fs::write(path, wav)
}

fn describe(descriptor: &Descriptor) {
    match descriptor {
        Descriptor::SequenceHeader(sh) => {
            println!(
                "sequence header: primary profile {}, additional profile {}",
                sh.primary_profile, sh.additional_profile
            );
        }
        Descriptor::CodecConfig(cc) => {
            println!(
                "codec config {}: {:?}, {} samples/frame, roll distance {}",
                cc.codec_config_id, cc.codec_id, cc.num_samples_per_frame, cc.audio_roll_distance
            );
            println!("  {:?}", cc.decoder_config);
        }
        Descriptor::AudioElement(ae) => {
            let kind = match &ae.config {
                AudioElementConfig::ChannelBased { layers } => {
                    format!("channel based, {} layer(s)", layers.len())
                }
                AudioElementConfig::AmbisonicsMono {
                    output_channel_count,
                    ..
                } => {
                    format!("ambisonics mono, {output_channel_count} channels")
                }
                AudioElementConfig::AmbisonicsProjection {
                    output_channel_count,
                    ..
                } => {
                    format!("ambisonics projection, {output_channel_count} channels")
                }
            };
            println!(
                "audio element {}: {kind}, codec config {}, substreams {:?}",
                ae.audio_element_id, ae.codec_config_id, ae.substream_ids
            );
        }
        Descriptor::MixPresentation(mp) => {
            println!(
                "mix presentation {}: {:?}",
                mp.mix_presentation_id, mp.localized_annotations
            );
            for (i, sub) in mp.sub_mixes.iter().enumerate() {
                let elements: Vec<u32> = sub.elements.iter().map(|e| e.audio_element_id).collect();
                let layouts: Vec<String> = sub
                    .layouts
                    .iter()
                    .map(|(layout, _)| match layout {
                        Layout::LoudspeakersSsConvention { sound_system } => {
                            format!("SS{sound_system}")
                        }
                        Layout::Binaural => "binaural".to_string(),
                        Layout::Reserved { layout_type } => format!("reserved{layout_type}"),
                    })
                    .collect();
                println!(
                    "  sub mix {i}: elements {elements:?}, output gain {} (Q7.8 dB), layouts {layouts:?}",
                    sub.output_mix_gain.default_mix_gain
                );
            }
        }
    }
}
