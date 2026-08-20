//! Inspects a standalone .iamf bitstream: OBU listing plus parsed
//! descriptor summaries.
//!
//! Will grow into a full decode-to-WAV tool (the Rust counterpart of
//! libiamf's iamfdec) as pipeline milestones land.

use std::process::ExitCode;

use iamf_obu::descriptors::{self, AudioElementConfig, Descriptor, Layout};
use iamf_obu::ObuIter;

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: iamfdec <file.iamf>");
        return ExitCode::FAILURE;
    };
    let data = match std::fs::read(&path) {
        Ok(data) => data,
        Err(err) => {
            eprintln!("error: cannot read {path}: {err}");
            return ExitCode::FAILURE;
        }
    };

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
