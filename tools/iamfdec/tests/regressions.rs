//! Regression tests for the October 2026 review findings (see
//! `docs/review-2026-10.md`). Each test builds a small LPCM stream by hand
//! that used to crash, hang, blow up memory or decode wrongly, and pins
//! the fixed behavior. The streams are tiny on purpose: every one of them
//! was reachable from untrusted input.

use iamf_codecs::DefaultFactory;
use iamf_dec::layout::SoundSystem;
use iamf_dec::presentation::{Descriptors, PresentationDecoder};
use iamf_dec::stream::{StreamDecoder, StreamSettings};
use iamf_obu::ObuIter;

/// Samples per frame of the default LPCM codec config.
const FRAME: usize = 64;

// -- Stream building ---------------------------------------------------

fn leb(mut value: u32) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return out;
        }
        out.push(byte | 0x80);
    }
}

fn obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
    obu_with_flags(obu_type, 0, payload)
}

fn obu_with_flags(obu_type: u8, flags: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![obu_type << 3 | flags];
    out.extend(leb(u32::try_from(payload.len()).unwrap()));
    out.extend_from_slice(payload);
    out
}

fn sequence_header() -> Vec<u8> {
    obu(31, b"iamf\x00\x00")
}

/// Codec config 0: little-endian 16-bit LPCM at `rate` Hz, `frame`
/// samples per frame.
fn lpcm_codec(rate: u32, frame: u32) -> Vec<u8> {
    let mut payload = vec![0];
    payload.extend(b"ipcm");
    payload.extend(leb(frame));
    payload.extend([0, 0, 1, 16]);
    payload.extend(rate.to_be_bytes());
    obu(0, &payload)
}

/// Mode-1 parameter definition at 48 kHz with a 0 dB default (the shape
/// of element and output mix gains).
fn gain_param(parameter_id: u32) -> Vec<u8> {
    let mut out = leb(parameter_id);
    out.extend(leb(48000));
    out.extend([0x80, 0, 0]);
    out
}

/// Channel-based element `id` on codec config 0 with one stereo layer
/// carried by substream `substream` (coupled).
fn stereo_element(id: u32, substream: u32) -> Vec<u8> {
    let mut payload = leb(id);
    payload.push(0); // channel-based
    payload.push(0); // codec config 0
    payload.extend(leb(1));
    payload.extend(leb(substream));
    payload.push(0); // no parameters
    payload.push(1 << 5); // num_layers 1
    payload.extend([1 << 4, 1, 1]); // stereo, 1 substream, 1 coupled
    obu(1, &payload)
}

/// Mix presentation `id` with one sub mix over `elements`, each rendered
/// with `headphones_mode`, and one stereo loudness layout.
fn mix(id: u32, elements: &[u32], headphones_mode: u8) -> Vec<u8> {
    let mut payload = leb(id);
    payload.push(0); // count_label
    payload.push(1); // num_sub_mixes
    payload.extend(leb(u32::try_from(elements.len()).unwrap()));
    for &element in elements {
        payload.extend(leb(element));
        payload.extend([headphones_mode << 6, 0]);
        payload.extend(gain_param(100 + element));
    }
    payload.extend(gain_param(200));
    payload.extend([1, 0x80, 0, 0, 0, 0, 0]);
    obu(2, &payload)
}

/// Sequence header, LPCM codec config, stereo element 1 (substream 0)
/// and mix 7.
fn stereo_descriptors(rate: u32, headphones_mode: u8) -> Vec<u8> {
    let mut out = sequence_header();
    out.extend(lpcm_codec(rate, FRAME as u32));
    out.extend(stereo_element(1, 0));
    out.extend(mix(7, &[1], headphones_mode));
    out
}

/// Interleaved s16le PCM of `frames` sample frames over `channels`.
fn pcm(frames: usize, channels: usize, value: i16) -> Vec<u8> {
    value.to_le_bytes().repeat(frames * channels)
}

/// Temporal delimiter plus one stereo audio frame on substream 0.
fn stereo_unit(value: i16) -> Vec<u8> {
    let mut out = obu(4, &[]);
    out.extend(obu(6, &pcm(FRAME, 2, value)));
    out
}

fn settings(layout: SoundSystem) -> StreamSettings {
    let mut settings = StreamSettings::default();
    settings.layout = layout;
    settings
}

fn stream_decoder(descriptors: &[u8], layout: SoundSystem) -> StreamDecoder {
    StreamDecoder::new_from_descriptors(descriptors, settings(layout), &DefaultFactory)
        .expect("descriptors accepted")
}

/// Pulls every ready unit, returning how many there were.
fn drain(decoder: &mut StreamDecoder) -> usize {
    let mut units = 0;
    while let Ok(Some(_)) = decoder.get_output_temporal_unit() {
        units += 1;
    }
    units
}

fn batch_decode(data: &[u8], layout: SoundSystem) -> Result<Vec<f32>, String> {
    let descriptors = Descriptors::collect(data).map_err(|e| e.to_string())?;
    let mut decoder = PresentationDecoder::new(&descriptors, 0, layout, &DefaultFactory)
        .map_err(|e| e.to_string())?;
    for obu in ObuIter::new(data) {
        let obu = obu.map_err(|e| e.to_string())?;
        decoder.process_obu(&obu).map_err(|e| e.to_string())?;
    }
    decoder
        .finish()
        .map(|mix| mix.interleaved)
        .map_err(|e| e.to_string())
}

// -- Sample rates ------------------------------------------------------

/// An LPCM sample rate of 0 with binaural output used to divide by zero
/// in the HRIR resampler (a process abort through the C ABI). Unsupported
/// rates are now rejected with the descriptors.
#[test]
fn lpcm_sample_rate_zero_rejected() {
    for rate in [0, 1, 4_800_000] {
        for mode in [0, 1] {
            let descriptors = stereo_descriptors(rate, mode);
            assert!(
                StreamDecoder::new_from_descriptors(
                    &descriptors,
                    settings(SoundSystem::Binaural),
                    &DefaultFactory
                )
                .is_err(),
                "rate {rate} mode {mode}"
            );
            let mut stream = descriptors;
            stream.extend(stereo_unit(100));
            assert!(batch_decode(&stream, SoundSystem::Binaural).is_err());
        }
    }
    // The supported rates still decode binaurally.
    let mut decoder = stream_decoder(&stereo_descriptors(16000, 1), SoundSystem::Binaural);
    decoder.decode(&stereo_unit(100)).unwrap();
    decoder.signal_end_of_decoding();
    assert_eq!(drain(&mut decoder), 1);
}
