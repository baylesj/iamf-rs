//! Fuzzes the C ABI — the exact surface an embedder (Chromium) calls:
//! create-from-descriptors, chunked decode, size-query and pull of
//! temporal units, getters, reset, destroy. Exercises the FFI argument
//! handling (null tolerance, buffer sizing) on top of everything
//! decode_stream covers.
//!
//! Run with `cargo +nightly fuzz run capi`. Seeds: `tests/vectors/*.iamf`.

#![no_main]

use iamf_rs::{
    IAMFRS_ERR_BUFFER_TOO_SMALL, IAMFRS_OK, IamfrsDecoder, IamfrsSettings,
    iamfrs_decoder_create_from_descriptors, iamfrs_decoder_decode, iamfrs_decoder_destroy,
    iamfrs_decoder_get_frame_size, iamfrs_decoder_get_num_output_channels,
    iamfrs_decoder_get_output_temporal_unit, iamfrs_decoder_get_sample_rate,
    iamfrs_decoder_get_sample_type, iamfrs_decoder_get_selected_layout,
    iamfrs_decoder_get_selected_mix_presentation_id, iamfrs_decoder_is_temporal_unit_available,
    iamfrs_decoder_reset, iamfrs_decoder_reset_with_new_mix, iamfrs_decoder_signal_end_of_decoding,
};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 3 {
        return;
    }
    // First bytes steer the settings so all layouts/orderings/types are hit.
    let settings = IamfrsSettings {
        output_layout: i32::from(data[0] % 15),
        sample_type: i32::from(data[1] % 3),
        mix_presentation_id: if data[1] & 4 == 0 {
            -1
        } else {
            i64::from(data[2])
        },
        channel_ordering: i32::from(data[1] & 1),
        disable_trim_start: data[1] >> 1 & 1,
        disable_trim_end: data[1] >> 2 & 1,
        requested_profiles: u32::from(data[2] & 0x07),
        enable_limiter: data[2] >> 3 & 1,
        enable_loudness_normalization: data[2] >> 4 & 1,
        loudness_target_db: -24.0,
    };
    let payload = &data[3..];

    // SAFETY: all pointers passed below are valid for the stated lengths,
    // and the handle is used single-threaded and destroyed exactly once.
    unsafe {
        let mut decoder: *mut IamfrsDecoder = std::ptr::null_mut();
        if iamfrs_decoder_create_from_descriptors(
            payload.as_ptr(),
            payload.len(),
            &settings,
            &mut decoder,
        ) != IAMFRS_OK
        {
            return;
        }

        let (mut channels, mut rate, mut frame, mut mix, mut layout, mut ty) =
            (0u32, 0u32, 0u32, 0u32, 0u32, 0u32);
        let _ = iamfrs_decoder_get_num_output_channels(decoder, &mut channels);
        let _ = iamfrs_decoder_get_sample_rate(decoder, &mut rate);
        let _ = iamfrs_decoder_get_frame_size(decoder, &mut frame);
        let _ = iamfrs_decoder_get_selected_mix_presentation_id(decoder, &mut mix);
        let _ = iamfrs_decoder_get_selected_layout(decoder, &mut layout);
        let _ = iamfrs_decoder_get_sample_type(decoder, &mut ty);

        // Feed in two chunks to exercise partial-OBU reassembly across the
        // FFI boundary, with a reset between pulls.
        let mid = payload.len() / 2;
        let _ = iamfrs_decoder_decode(decoder, payload.as_ptr(), mid);
        let _ = iamfrs_decoder_decode(decoder, payload[mid..].as_ptr(), payload.len() - mid);
        let _ = iamfrs_decoder_signal_end_of_decoding(decoder);

        let mut scratch = vec![0u8; 1 << 16];
        let mut produced = 0usize;
        while iamfrs_decoder_is_temporal_unit_available(decoder) == 1 && produced < 1 << 24 {
            let mut needed = 0usize;
            // Size query with zero capacity, then the real pull.
            match iamfrs_decoder_get_output_temporal_unit(
                decoder,
                std::ptr::null_mut(),
                0,
                &mut needed,
            ) {
                IAMFRS_ERR_BUFFER_TOO_SMALL => {
                    if needed > scratch.len() {
                        scratch.resize(needed.min(1 << 24), 0);
                    }
                    let mut written = 0usize;
                    if iamfrs_decoder_get_output_temporal_unit(
                        decoder,
                        scratch.as_mut_ptr(),
                        scratch.len(),
                        &mut written,
                    ) != IAMFRS_OK
                    {
                        break;
                    }
                    produced += written.max(1);
                }
                IAMFRS_OK => produced += needed.max(1),
                _ => break,
            }
        }
        let _ = iamfrs_decoder_reset(decoder);
        let _ = iamfrs_decoder_decode(decoder, payload.as_ptr(), mid);
        // In-place mix/layout switch, then keep decoding on the handle.
        let _ = iamfrs_decoder_reset_with_new_mix(
            decoder,
            if data[2] & 0x20 == 0 {
                -1
            } else {
                i64::from(data[0])
            },
            i32::from(data[2] % 16) - 1,
        );
        let _ = iamfrs_decoder_decode(decoder, payload[mid..].as_ptr(), payload.len() - mid);
        let _ = iamfrs_decoder_is_temporal_unit_available(decoder);
        iamfrs_decoder_destroy(decoder);
    }
});
