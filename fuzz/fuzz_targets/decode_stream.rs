//! Fuzzes the full streaming decoder over arbitrary bytes: descriptor
//! parsing, parameter blocks, codec decode (all codecs), demixing,
//! rendering (including binaural and IAMF v2.0 objects and two sub
//! mixes), limiting, output assembly, reset and in-place mix switching
//! must never panic. Output volume is capped so hostile inputs can't OOM
//! the harness.
//!
//! Run with `cd fuzz && cargo fuzz run decode_stream` (the nightly is
//! pinned in fuzz/rust-toolchain.toml). Seed the corpus with test vectors
//! (`cp ../tests/vectors/*.iamf corpus/decode_stream/`).

#![no_main]

use iamf_codecs::DefaultFactory;
use iamf_dec::layout::SoundSystem;
use iamf_dec::stream::{ChannelOrdering, MixSelection, StreamDecoder, StreamSettings};
use libfuzzer_sys::fuzz_target;

/// Pulls every available unit, up to a cap on total output.
fn drain(decoder: &mut StreamDecoder, produced: &mut usize) {
    while decoder.is_temporal_unit_available() && *produced < 1 << 24 {
        match decoder.get_output_temporal_unit() {
            Ok(Some(unit)) => *produced += unit.len().max(1),
            Ok(None) => break,
            // Errors skip the unit; keep pulling like a real caller.
            Err(_) => *produced += 1,
        }
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }
    // The first bytes steer the settings so every layout, ordering,
    // profile set and post-processing option gets coverage.
    let layout = SoundSystem::from_u8(data[0] % 15).expect("0..=14 are valid");
    let mut settings = StreamSettings::default();
    settings.layout = Some(layout);
    settings.sample_type = None;
    settings.channel_ordering = if data[1] & 1 == 0 {
        ChannelOrdering::Iamf
    } else {
        ChannelOrdering::Android
    };
    settings.trimming.trim_beginning = data[1] & 0x02 == 0;
    settings.trimming.trim_end = data[1] & 0x04 == 0;
    settings.enable_limiter = data[1] & 0x10 != 0;
    settings.loudness_target_db = (data[1] & 0x20 != 0).then_some(-24.0);
    // All six profile bits, so the IAMF v2.0 paths are reachable.
    settings.requested_profiles =
        iamf_dec::profile::ProfileSet::from_bits(u32::from(data[2] & 0x3f));
    settings.mix_selection = match data[2] >> 6 {
        0 => MixSelection::Auto,
        1 => MixSelection::ByIndex(usize::from(data[3] & 3)),
        _ => MixSelection::ById(u32::from(data[3])),
    };
    let payload = &data[4..];
    let Ok(mut decoder) = StreamDecoder::new_from_descriptors(payload, settings, &DefaultFactory)
    else {
        return;
    };
    let mut produced = 0usize;
    // Feed in uneven chunks to exercise partial-OBU reassembly, pulling
    // between pushes.
    let split = |n: usize| (payload.len() * n / 3).min(payload.len());
    let _ = decoder.decode(&payload[..split(1)]);
    drain(&mut decoder, &mut produced);
    let _ = decoder.decode(&payload[split(1)..split(2)]);
    // Switch mix and layout in place, or seek, mid-stream.
    if data[3] & 0x80 != 0 {
        let next = SoundSystem::from_u8(data[3] % 15);
        let _ = decoder.reset_with_new_mix(MixSelection::Auto, next, &DefaultFactory);
    } else if data[3] & 0x40 != 0 {
        decoder.reset();
    }
    let _ = decoder.decode(&payload[split(2)..]);
    decoder.signal_end_of_decoding();
    drain(&mut decoder, &mut produced);
});
