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

// -- Decoded frame shape -----------------------------------------------

/// Stereo element `id` on codec config 0 carried as two mono substreams
/// (`substreams`), one stereo layer without coupling.
fn dual_mono_element(id: u32, substreams: [u32; 2]) -> Vec<u8> {
    let mut payload = leb(id);
    payload.extend([0, 0]);
    payload.extend(leb(2));
    for substream in substreams {
        payload.extend(leb(substream));
    }
    payload.push(0);
    payload.push(1 << 5);
    payload.extend([1 << 4, 2, 0]); // stereo, 2 substreams, 0 coupled
    obu(1, &payload)
}

/// Substreams of one temporal unit that decode to different lengths used
/// to make the batch decoder index past the end of a plane
/// (presentation.rs `plane[kept]`, `binauralize`) and the stream decoder
/// silently pad or truncate. Every decoded frame must now hold exactly
/// `num_samples_per_frame` samples per channel.
#[test]
fn decoded_frame_length_must_match_codec_config() {
    let mut descriptors = sequence_header();
    descriptors.extend(lpcm_codec(48000, FRAME as u32));
    descriptors.extend(dual_mono_element(1, [0, 1]));
    descriptors.extend(mix(7, &[1], 0));
    for (left, right) in [(FRAME, FRAME / 2), (FRAME / 2, FRAME), (FRAME, 2 * FRAME)] {
        let mut stream = descriptors.clone();
        stream.extend(obu(4, &[]));
        stream.extend(obu(6, &pcm(left, 1, 1000)));
        stream.extend(obu(7, &pcm(right, 1, 1000)));
        for layout in [SoundSystem::A, SoundSystem::B, SoundSystem::Binaural] {
            let err = batch_decode(&stream, layout).unwrap_err();
            assert!(err.contains("expected 64 samples"), "{err}");
            let mut decoder = stream_decoder(&descriptors, layout);
            assert!(decoder.decode(&stream[descriptors.len()..]).is_err());
        }
    }
    // Well-formed units still decode.
    let mut stream = descriptors.clone();
    stream.extend(obu(4, &[]));
    stream.extend(obu(6, &pcm(FRAME, 1, 1000)));
    stream.extend(obu(7, &pcm(FRAME, 1, 1000)));
    assert_eq!(
        batch_decode(&stream, SoundSystem::A).unwrap().len(),
        2 * FRAME
    );
}

// -- Stream decoder error recovery ---------------------------------------

/// A failing OBU used to stay at the head of the input buffer, so every
/// later `decode()` re-parsed it and failed again: one corrupt LPCM frame
/// ended playback for good.
#[test]
fn stream_recovers_after_a_corrupt_frame() {
    let descriptors = stereo_descriptors(48000, 0);
    let mut decoder = stream_decoder(&descriptors, SoundSystem::A);
    let mut bad = obu(4, &[]);
    bad.extend(obu(6, &[0; 5])); // not a whole number of sample frames
    assert!(decoder.decode(&bad).is_err());
    for _ in 0..3 {
        decoder.decode(&stereo_unit(100)).unwrap();
    }
    decoder.signal_end_of_decoding();
    assert_eq!(drain(&mut decoder), 3);
}

/// A 2-byte OBU whose header promises trimming or extension fields that
/// its empty body lacks used to read as "incomplete", so the decoder
/// returned Ok forever, produced nothing and buffered all later input.
#[test]
fn stream_recovers_after_a_too_short_obu() {
    for short in [[(6 << 3) | 0x02, 0x00], [0x01, 0x00]] {
        let descriptors = stereo_descriptors(48000, 0);
        let mut decoder = stream_decoder(&descriptors, SoundSystem::A);
        decoder.decode(&stereo_unit(100)).unwrap();
        assert!(decoder.decode(&short).is_err());
        for _ in 0..3 {
            decoder.decode(&stereo_unit(100)).unwrap();
        }
        decoder.signal_end_of_decoding();
        assert_eq!(drain(&mut decoder), 4, "{short:x?}");
    }
}

/// An OBU whose outer framing is broken can't be skipped; the buffered
/// input is dropped and decoding resumes with the next push.
#[test]
fn stream_drops_input_it_cannot_resynchronize() {
    let descriptors = stereo_descriptors(48000, 0);
    let mut decoder = stream_decoder(&descriptors, SoundSystem::A);
    // obu_size is an over-long leb128 (9 continuation bytes).
    assert!(
        decoder
            .decode(&[0x30, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff])
            .is_err()
    );
    decoder.decode(&stereo_unit(100)).unwrap();
    decoder.signal_end_of_decoding();
    assert_eq!(drain(&mut decoder), 1);
}

/// Stereo elements 1..=`count` (element n on substream n - 1) in one mix.
fn multi_element_descriptors(count: u32) -> Vec<u8> {
    let mut out = sequence_header();
    out.extend(lpcm_codec(48000, FRAME as u32));
    for id in 1..=count {
        out.extend(stereo_element(id, id - 1));
    }
    out.extend(mix(7, &(1..=count).collect::<Vec<_>>(), 0));
    out
}

/// Audio frame OBU for `substream` (0..=17) with explicit trims.
fn trimmed_frame(substream: u8, value: i16, trim_start: u32, trim_end: u32) -> Vec<u8> {
    let mut payload = leb(trim_end);
    payload.extend(leb(trim_start));
    payload.extend(pcm(FRAME, 2, value));
    obu_with_flags(6 + substream, 0x02, &payload)
}

/// A unit that failed in `get_output_temporal_unit` used to be popped
/// from the elements before the failure but not from the ones after, so
/// for the rest of the stream elements mixed audio from different
/// temporal units. A failing unit is now skipped by every element.
#[test]
fn failed_unit_is_skipped_by_every_element() {
    // Three elements: the middle one fails, so the first had already
    // been popped and the last had not.
    let descriptors = multi_element_descriptors(3);
    let mut decoder = stream_decoder(&descriptors, SoundSystem::A);
    let mut stream = Vec::new();
    for unit in 1..=4i16 {
        stream.extend(obu(4, &[]));
        // Unit 2's frames disagree on trimming (§3.9), so it fails.
        let trim = if unit == 2 { 8 } else { 0 };
        stream.extend(trimmed_frame(0, 100 * unit, 0, 0));
        stream.extend(trimmed_frame(1, 100 * unit, trim, 0));
        stream.extend(trimmed_frame(2, 100 * unit, 0, 0));
    }
    decoder.decode(&stream).unwrap();
    decoder.signal_end_of_decoding();
    let mut firsts = Vec::new();
    loop {
        match decoder.get_output_temporal_unit() {
            Ok(Some(unit)) => firsts.push(i16::from_le_bytes([unit[0], unit[1]])),
            Ok(None) => break,
            Err(_) => firsts.push(0),
        }
    }
    // All elements always contribute the same unit: 3 x 100 x unit.
    assert_eq!(firsts, [300, 0, 900, 1200]);
}

/// A failed `reset_with_new_mix` used to drop the parsed descriptors, so
/// every later reconfiguration failed with "no mix presentations",
/// although the API (and the C header) say the call can be retried.
#[test]
fn failed_reset_with_new_mix_can_be_retried() {
    use iamf_dec::stream::MixSelection;
    let descriptors = stereo_descriptors(48000, 0);
    let mut decoder = stream_decoder(&descriptors, SoundSystem::A);
    assert!(
        decoder
            .reset_with_new_mix(MixSelection::ByIndex(7), None, &DefaultFactory)
            .is_err()
    );
    assert_eq!(
        decoder
            .reset_with_new_mix(MixSelection::Auto, Some(SoundSystem::B), &DefaultFactory)
            .unwrap(),
        (7, SoundSystem::B)
    );
    decoder.decode(&stereo_unit(100)).unwrap();
    decoder.signal_end_of_decoding();
    assert_eq!(drain(&mut decoder), 1);
}

/// A temporal unit that never completes used to buffer decoded frames
/// without limit (12 KB of input grew to 65 MB). A substream may now be
/// at most one frame ahead: a frame that starts a new unit before the
/// previous one completed drops the incomplete unit.
#[test]
fn incomplete_units_do_not_accumulate() {
    let mut descriptors = sequence_header();
    descriptors.extend(lpcm_codec(48000, FRAME as u32));
    descriptors.extend(dual_mono_element(1, [0, 1]));
    descriptors.extend(mix(7, &[1], 0));
    let mut decoder = stream_decoder(&descriptors, SoundSystem::A);
    // Substream 1 never arrives (and no temporal delimiters).
    decoder.decode(&obu(6, &pcm(FRAME, 1, 100))).unwrap();
    for _ in 0..1000 {
        assert!(decoder.decode(&obu(6, &pcm(FRAME, 1, 100))).is_err());
    }
    // Only the newest substream-0 frame is buffered: one more frame for
    // substream 1 completes exactly one unit.
    decoder.decode(&obu(7, &pcm(FRAME, 1, 100))).unwrap();
    decoder.signal_end_of_decoding();
    assert_eq!(drain(&mut decoder), 1);
}

/// A temporal delimiter in the middle of a unit used to fail on every
/// later delimiter too; the incomplete unit is now dropped.
#[test]
fn misaligned_delimiter_resynchronizes() {
    let mut descriptors = sequence_header();
    descriptors.extend(lpcm_codec(48000, FRAME as u32));
    descriptors.extend(dual_mono_element(1, [0, 1]));
    descriptors.extend(mix(7, &[1], 0));
    let mut decoder = stream_decoder(&descriptors, SoundSystem::A);
    let unit = |out: &mut Vec<u8>| {
        out.extend(obu(4, &[]));
        out.extend(obu(6, &pcm(FRAME, 1, 100)));
        out.extend(obu(7, &pcm(FRAME, 1, 100)));
    };
    let mut lossy = Vec::new();
    lossy.extend(obu(4, &[]));
    lossy.extend(obu(6, &pcm(FRAME, 1, 100))); // substream 1 lost
    assert!(decoder.decode(&lossy).is_ok());
    let mut next = Vec::new();
    unit(&mut next);
    assert!(decoder.decode(&next).is_err()); // reports the lost unit
    let mut rest = Vec::new();
    unit(&mut rest);
    unit(&mut rest);
    decoder.decode(&rest).unwrap();
    decoder.signal_end_of_decoding();
    assert_eq!(drain(&mut decoder), 3);
}

/// An element used twice in a mix used to receive frames only in its
/// first use, so the stream decoder stalled forever (and the batch
/// decoder failed). Both uses now decode every frame.
#[test]
fn element_used_twice_decodes() {
    let mut descriptors = sequence_header();
    descriptors.extend(lpcm_codec(48000, FRAME as u32));
    descriptors.extend(stereo_element(1, 0));
    descriptors.extend(mix(7, &[1, 1], 0));
    let mut decoder = stream_decoder(&descriptors, SoundSystem::A);
    decoder.decode(&stereo_unit(100)).unwrap();
    decoder.signal_end_of_decoding();
    let unit = decoder
        .get_output_temporal_unit()
        .unwrap()
        .expect("one unit");
    assert_eq!(i16::from_le_bytes([unit[0], unit[1]]), 200);
    let mut stream = descriptors;
    stream.extend(stereo_unit(100));
    let batch = batch_decode(&stream, SoundSystem::A).unwrap();
    assert_eq!(batch.len(), 2 * FRAME);
    assert!((batch[0] - 200.0 / 32768.0).abs() < 1e-6, "{}", batch[0]);
}

/// Two different elements of a mix that share a substream ID can't both
/// be fed; the mix is rejected instead of stalling.
#[test]
fn elements_sharing_a_substream_rejected() {
    let mut descriptors = sequence_header();
    descriptors.extend(lpcm_codec(48000, FRAME as u32));
    descriptors.extend(stereo_element(1, 0));
    descriptors.extend(stereo_element(2, 0));
    descriptors.extend(mix(7, &[1, 2], 0));
    let err = StreamDecoder::new_from_descriptors(
        &descriptors,
        settings(SoundSystem::A),
        &DefaultFactory,
    )
    .unwrap_err();
    assert!(err.to_string().contains("share substream 0"), "{err}");
    let mut stream = descriptors;
    stream.extend(stereo_unit(100));
    let err = batch_decode(&stream, SoundSystem::A).unwrap_err();
    assert!(err.contains("share substream 0"), "{err}");
}
