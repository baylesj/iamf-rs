//! Fuzzes the full streaming decoder over arbitrary bytes: descriptor
//! parsing, parameter blocks, codec decode (all codecs), demixing,
//! rendering (including binaural), and output assembly must never panic.
//! Output volume is capped so hostile inputs can't OOM the harness.
//!
//! Run with `cargo +nightly fuzz run decode_stream`. Seed the corpus with
//! test vectors (`cp tests/vectors/*.iamf fuzz/corpus/decode_stream/`).

#![no_main]

use iamf_codecs::DefaultFactory;
use iamf_dec::layout::SoundSystem;
use iamf_dec::stream::{StreamDecoder, StreamSettings};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    // First bytes steer the settings so all layouts/orderings get coverage.
    let layout = SoundSystem::from_u8(data[0] % 15).expect("0..=14 are valid");
    let ordering = if data[1] & 1 == 0 {
        iamf_dec::stream::ChannelOrdering::Iamf
    } else {
        iamf_dec::stream::ChannelOrdering::Android
    };
    let settings = StreamSettings {
        layout,
        sample_type: None,
        channel_ordering: ordering,
        ..StreamSettings::default()
    };
    let payload = &data[2..];
    let Ok(mut decoder) = StreamDecoder::new_from_descriptors(payload, settings, &DefaultFactory)
    else {
        return;
    };
    // Feed in two chunks to exercise partial-OBU reassembly.
    let mid = payload.len() / 2;
    let _ = decoder.decode(&payload[..mid]);
    let _ = decoder.decode(&payload[mid..]);
    decoder.signal_end_of_decoding();
    let mut produced = 0usize;
    while decoder.is_temporal_unit_available() && produced < 1 << 24 {
        match decoder.get_output_temporal_unit() {
            Ok(Some(unit)) => produced += unit.len().max(1),
            _ => break,
        }
    }
});
