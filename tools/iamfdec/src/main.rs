//! Inspects a standalone .iamf bitstream: OBU listing plus parsed
//! descriptor summaries.
//!
//! Will grow into a full decode-to-WAV tool (the Rust counterpart of
//! libiamf's iamfdec) as pipeline milestones land.

use std::process::ExitCode;

use iamf_codecs::DefaultFactory;
use iamf_dec::element::ElementDecoder;
use iamf_obu::descriptors::{self, AudioElementConfig, Descriptor, Layout};
use iamf_obu::{AudioFrame, ObuIter};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (path, wav_out) = match args.as_slice() {
        [path] => (path.clone(), None),
        [path, flag, out] if flag == "-o" => (path.clone(), Some(out.clone())),
        _ => {
            eprintln!("usage: iamfdec <file.iamf> [-o out.wav]");
            return ExitCode::FAILURE;
        }
    };
    let data = match std::fs::read(&path) {
        Ok(data) => data,
        Err(err) => {
            eprintln!("error: cannot read {path}: {err}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(out) = wav_out {
        return decode_to_wav(&data, &out);
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

/// Decodes the first audio element substream-by-substream and writes the
/// concatenated substream channels as a 16-bit WAV. Channel ordering is
/// substream order — real layout mapping arrives with the renderer
/// (milestone 4).
fn decode_to_wav(data: &[u8], out_path: &str) -> ExitCode {
    let mut codec_config = None;
    let mut element = None;
    let mut decoder: Option<ElementDecoder> = None;
    let mut frames = 0usize;

    for result in ObuIter::new(data) {
        let obu = match result {
            Ok(obu) => obu,
            Err(err) => {
                eprintln!("error: {err}");
                return ExitCode::FAILURE;
            }
        };

        match descriptors::parse(&obu) {
            Ok(Some(Descriptor::CodecConfig(cc))) => codec_config = Some(cc),
            Ok(Some(Descriptor::AudioElement(ae))) if element.is_none() => element = Some(ae),
            Ok(Some(_)) | Ok(None) => {}
            Err(err) => {
                eprintln!("error: {:?}: {err}", obu.header.obu_type);
                return ExitCode::FAILURE;
            }
        }

        let frame = match AudioFrame::from_obu(&obu) {
            Ok(Some(frame)) => frame,
            Ok(None) => continue,
            Err(err) => {
                eprintln!("error: audio frame: {err}");
                return ExitCode::FAILURE;
            }
        };

        if decoder.is_none() {
            let (Some(cc), Some(ae)) = (&codec_config, &element) else {
                eprintln!("error: audio frame before descriptors");
                return ExitCode::FAILURE;
            };
            decoder = match ElementDecoder::new(ae, cc, &DefaultFactory) {
                Ok(d) => Some(d),
                Err(err) => {
                    eprintln!("error: {err}");
                    return ExitCode::FAILURE;
                }
            };
        }
        match decoder.as_mut().unwrap().decode_frame(&frame) {
            Ok(consumed) => frames += usize::from(consumed),
            Err(err) => {
                eprintln!("error: substream {}: {err}", frame.substream_id);
                return ExitCode::FAILURE;
            }
        }
    }

    let Some(decoder) = decoder else {
        eprintln!("error: no audio frames found");
        return ExitCode::FAILURE;
    };
    let substreams = decoder.finish();
    let sample_rate = substreams[0].sample_rate;
    let total_channels: usize = substreams.iter().map(|s| usize::from(s.channels)).sum();
    let frame_count = substreams
        .iter()
        .map(|s| s.samples.len() / usize::from(s.channels).max(1))
        .min()
        .unwrap_or(0);

    let mut interleaved = vec![0.0f32; frame_count * total_channels];
    let mut channel_offset = 0usize;
    for sub in &substreams {
        let ch = usize::from(sub.channels);
        for t in 0..frame_count {
            interleaved[t * total_channels + channel_offset..][..ch]
                .copy_from_slice(&sub.samples[t * ch..][..ch]);
        }
        channel_offset += ch;
    }

    if let Err(err) = write_wav_s16(out_path, &interleaved, total_channels as u16, sample_rate) {
        eprintln!("error: writing {out_path}: {err}");
        return ExitCode::FAILURE;
    }
    println!(
        "decoded {frames} frames -> {frame_count} samples x {total_channels} ch @ {sample_rate} Hz -> {out_path}"
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
        let clamped = (sample.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        wav.extend(clamped.to_le_bytes());
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
