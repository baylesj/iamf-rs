//! Codec inputs that once panicked inside a third-party decoder.
//!
//! The adapters catch codec panics, but `catch_unwind` does nothing in
//! `panic = "abort"` builds such as Chromium's, so a caught panic still
//! kills the process there. These tests fail on any panic, caught or not.
//! They live in their own test binary because the panic hook they install
//! is process-wide.

use std::sync::atomic::{AtomicUsize, Ordering};

use iamf_codecs::DefaultFactory;
use iamf_dec::layout::SoundSystem;
use iamf_dec::profile::ProfileSet;
use iamf_dec::stream::{MixSelection, StreamDecoder, StreamSettings};

static PANICS: AtomicUsize = AtomicUsize::new(0);

/// Decodes `payload` the way the `decode_stream` fuzz target does (three
/// uneven pushes, then end of stream, pulling every unit) and returns how
/// many panics happened anywhere, including ones the adapters caught.
fn panics_while_decoding(payload: &[u8], settings: StreamSettings) -> usize {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {
        PANICS.fetch_add(1, Ordering::SeqCst);
    }));
    PANICS.store(0, Ordering::SeqCst);
    if let Ok(mut decoder) = StreamDecoder::new_from_descriptors(payload, settings, &DefaultFactory)
    {
        let third = payload.len() / 3;
        for chunk in [
            &payload[..third],
            &payload[third..2 * third],
            &payload[2 * third..],
        ] {
            let _ = decoder.decode(chunk);
            while let Ok(Some(_)) = decoder.get_output_temporal_unit() {}
        }
        decoder.signal_end_of_decoding();
        while let Ok(Some(_)) = decoder.get_output_temporal_unit() {}
    }
    std::panic::set_hook(previous);
    PANICS.load(Ordering::SeqCst)
}

/// An AAC-LC channel pair whose `max_sfb` exceeds the scale-factor bands
/// for its sample rate. symphonia 0.5 indexed past its band table; 0.6
/// rejects the frame. Found by the nightly `decode_stream` fuzzer.
#[test]
fn aac_max_sfb_past_band_table() {
    #[rustfmt::skip]
    let payload: &[u8] = &[
        0, 0, 0, 0, 0, 14, 34, 105, 112, 99, 109, 8, 0, 0, 1, 16, 0, 0, 187,
        128, 8, 11, 172, 2, 0, 222, 254, 0, 0, 0, 16, 1, 1, 0, 0, 52, 34, 6,
        0, 1, 0, 255, 255, 2, 0, 254, 255, 3, 0, 253, 255, 4, 0, 252, 191, 5,
        0, 251, 255, 6, 0, 250, 255, 7, 0, 249, 255, 8, 0, 248, 255, 53, 34,
        6, 0, 1, 0, 190, 255, 2, 0, 254, 255, 3, 255, 4, 0, 253, 0, 252, 191,
        5, 0, 251, 255, 6, 0, 250, 255, 7, 0, 249, 255, 8, 0, 248, 255, 52, 8,
        50, 111, 248, 6, 105, 97, 109, 102, 0, 0, 0, 29, 200, 1, 109, 112, 52,
        97, 128, 8, 255, 255, 4, 17, 64, 21, 0, 0, 0, 0, 0, 0, 0, 0, 0, 40, 0,
        5, 2, 17, 144, 8, 12, 172, 2, 0, 200, 1, 1, 0, 0, 32, 16, 1, 1, 16, 80,
        42, 1, 101, 110, 45, 117, 115, 116, 155, 116, 115, 0, 95, 109, 105,
        120, 95, 112, 114, 101, 115, 0, 1, 1, 172, 2, 116, 101, 115, 116, 95,
        115, 117, 98, 95, 109, 105, 120, 95, 48, 95, 97, 117, 156, 147, 111,
        95, 101, 108, 101, 109, 101, 110, 116, 95, 48, 0, 0, 0, 100, 128, 247,
        2, 128, 0, 0, 100, 128, 247, 2, 128, 0, 0, 1, 128, 0, 234, 239, 234,
        238, 24, 8, 100, 128, 8, 128, 4, 75, 75, 192,
    ];
    // The fuzz target's settings for this input: 5.1, IAMF channel order,
    // no start trimming, every profile bit clear.
    let mut settings = StreamSettings::default();
    settings.layout = Some(SoundSystem::B);
    settings.sample_type = None;
    settings.trimming.trim_beginning = false;
    settings.requested_profiles = ProfileSet::from_bits(0);
    settings.mix_selection = MixSelection::Auto;
    assert_eq!(panics_while_decoding(payload, settings), 0);
}
