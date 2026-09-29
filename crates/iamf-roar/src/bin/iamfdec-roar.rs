//! Decodes a standalone .iamf file to 16-bit WAV, rendering through ROAR
//! (AOM's Rust Open Audio Renderer) instead of iamf-dec's builtin
//! renderer. The options match `iamfdec`'s decode options.
//!
//! Loudness normalization and the limiter run inside the streaming
//! decoder, on the float signal before quantization, so limiting sees the
//! real peaks.

use std::process::ExitCode;
use std::sync::Arc;

use iamf_codecs::DefaultFactory;
use iamf_dec::layout::SoundSystem;
use iamf_dec::stream::{
    MixSelection, OutputSampleType, StreamDecoder, StreamSettings, TrimmingSettings,
};
use iamf_roar::RoarRendererFactory;

/// Sound system number of binaural output (iamf-tools `kIAMF_Binaural`).
const BINAURAL: u8 = 14;

const USAGE: &str = "usage: iamfdec-roar <file.iamf> -o out.wav [-s sound_system|binaural] \
     [--limiter] [--loudness dB] [--no-trim-start] [--no-trim-end]";

struct Options {
    sound_system: u8,
    limiter: bool,
    /// Target loudness in dB for normalization, when set.
    loudness: Option<f32>,
    /// Which audio-frame trims to apply (iamf-tools `TrimmingSettings`).
    trimming: TrimmingSettings,
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut opts = Options {
        sound_system: 0,
        limiter: false,
        loudness: None,
        trimming: TrimmingSettings::default(),
    };
    let mut wav_out = None;
    let mut path = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-o" => {
                if let Some(out) = args.next() {
                    wav_out = Some(out);
                } else {
                    eprintln!("error: -o expects an output path");
                    return ExitCode::FAILURE;
                }
            }
            "-s" => {
                let parsed = match args.next().as_deref() {
                    Some("binaural") => Some(BINAURAL),
                    Some(v) => v.parse().ok(),
                    None => None,
                };
                if let Some(s) = parsed {
                    opts.sound_system = s;
                } else {
                    eprintln!("error: -s expects a sound system number (0..=14) or `binaural`");
                    return ExitCode::FAILURE;
                }
            }
            "--limiter" => opts.limiter = true,
            "--loudness" => {
                if let Some(db) = args.next().and_then(|v| v.parse().ok()) {
                    opts.loudness = Some(db);
                } else {
                    eprintln!("error: --loudness expects a dB value (e.g. -24)");
                    return ExitCode::FAILURE;
                }
            }
            "--no-trim-start" => opts.trimming.trim_beginning = false,
            "--no-trim-end" => opts.trimming.trim_end = false,
            flag if flag.starts_with('-') => {
                eprintln!("error: unknown option {flag}\n{USAGE}");
                return ExitCode::FAILURE;
            }
            _ if path.is_none() => path = Some(arg),
            _ => {
                eprintln!("{USAGE}");
                return ExitCode::FAILURE;
            }
        }
    }
    let (Some(path), Some(out_path)) = (path, wav_out) else {
        eprintln!("{USAGE}");
        return ExitCode::FAILURE;
    };
    let data = match std::fs::read(&path) {
        Ok(data) => data,
        Err(err) => {
            eprintln!("error: cannot read {path}: {err}");
            return ExitCode::FAILURE;
        }
    };
    match decode_to_wav(&data, &out_path, &opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Decodes the mix presentation §7.4.1 selects for the target sound system
/// through ROAR and writes 16-bit WAV.
fn decode_to_wav(data: &[u8], out_path: &str, opts: &Options) -> Result<(), String> {
    let sound_system = opts.sound_system;
    let target = SoundSystem::from_u8(sound_system)
        .ok_or_else(|| format!("unknown sound system {sound_system}"))?;
    let mut settings = StreamSettings::default();
    settings.layout = target;
    settings.mix_selection = MixSelection::Auto;
    settings.sample_type = Some(OutputSampleType::Int16LittleEndian);
    settings.trimming = opts.trimming;
    // Loudness and limiting run in the decoder, before quantization.
    settings.loudness_target_db = opts.loudness;
    settings.enable_limiter = opts.limiter;
    let mut decoder = StreamDecoder::new_from_descriptors_with_renderer(
        data,
        settings,
        &DefaultFactory,
        Arc::new(RoarRendererFactory::new()),
    )
    .map_err(|e| e.to_string())?;
    decoder.decode(data).map_err(|e| e.to_string())?;
    decoder.signal_end_of_decoding();
    let mut units = 0usize;
    let mut pcm = Vec::new();
    while let Some(bytes) = decoder
        .get_output_temporal_unit()
        .map_err(|e| e.to_string())?
    {
        units += 1;
        pcm.extend_from_slice(&bytes);
    }
    let channels = decoder.num_output_channels();
    let sample_rate = decoder.sample_rate();
    write_wav_s16(out_path, &pcm, channels as u16, sample_rate)
        .map_err(|e| format!("writing {out_path}: {e}"))?;
    let frame_count = pcm.len() / 2 / channels.max(1);
    println!(
        "rendered {units} temporal units -> {frame_count} samples x {channels} ch @ {sample_rate} Hz (sound system {sound_system}, ROAR) -> {out_path}"
    );
    Ok(())
}

/// Writes interleaved s16le PCM bytes as a WAV file.
fn write_wav_s16(path: &str, pcm: &[u8], channels: u16, sample_rate: u32) -> std::io::Result<()> {
    let data_len = pcm.len() as u32;
    let byte_rate = sample_rate * u32::from(channels) * 2;
    let mut wav = Vec::with_capacity(44 + pcm.len());
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
    wav.extend_from_slice(pcm);
    std::fs::write(path, wav)
}
