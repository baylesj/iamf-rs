//! C ABI over [`iamf_dec::stream::StreamDecoder`], shaped after the
//! iamf-tools iterative decoder API that Chromium's `IamfAudioDecoder`
//! consumes (create-from-descriptors, push bytes, pull temporal units).
//!
//! See `include/iamf_rs.h` for the C declarations. All functions return
//! `IAMFRS_OK` (0) on success or a negative `iamfrs_status` on failure,
//! and tolerate null pointers by returning `IAMFRS_ERR_INVALID_ARG`.
//!
//! Thread safety matches iamf-tools: one decoder instance must be used
//! from one thread at a time; distinct instances are independent.

#![deny(unsafe_op_in_unsafe_fn)]

use std::ffi::c_int;

use iamf_codecs::DefaultFactory;
use iamf_dec::layout::SoundSystem;
use iamf_dec::stream::{OutputSampleType, StreamDecoder, StreamSettings};
use iamf_dec::DecodeError;

pub const IAMFRS_OK: c_int = 0;
pub const IAMFRS_ERR_INVALID_ARG: c_int = -1;
pub const IAMFRS_ERR_UNSUPPORTED: c_int = -2;
pub const IAMFRS_ERR_CORRUPT_DATA: c_int = -3;
pub const IAMFRS_ERR_BUFFER_TOO_SMALL: c_int = -4;
pub const IAMFRS_ERR_NO_TEMPORAL_UNIT: c_int = -5;
pub const IAMFRS_ERR_INTERNAL: c_int = -6;

fn status_of(err: &DecodeError) -> c_int {
    match err {
        DecodeError::UnsupportedCodec | DecodeError::Unimplemented(_) => IAMFRS_ERR_UNSUPPORTED,
        DecodeError::CorruptPacket(_) | DecodeError::InvalidDescriptors(_) => {
            IAMFRS_ERR_CORRUPT_DATA
        }
    }
}

/// Opaque decoder handle.
pub struct IamfrsDecoder {
    inner: StreamDecoder,
    /// A rendered unit that did not fit the caller's buffer yet.
    pending_unit: Option<Vec<u8>>,
}

/// Decoder configuration, mirroring iamf-tools' `IamfDecoderFactory::Settings`.
#[repr(C)]
pub struct IamfrsSettings {
    /// IAMF sound system numbering shared with iamf-tools `OutputLayout`
    /// (0 = stereo ... 13 = 9.1.6, 14 = binaural).
    pub output_layout: i32,
    /// 0 = auto (from the stream's bit depth), 1 = s16le, 2 = s32le.
    pub sample_type: i32,
    /// Mix presentation to decode, or -1 to select automatically (a mix
    /// declaring the requested layout, else the first).
    pub mix_presentation_id: i64,
    /// 0 = IAMF rendering order, 1 = Android/WAVE order (iamf-tools
    /// `ChannelOrdering`).
    pub channel_ordering: i32,
    /// Nonzero disables trimming of num_samples_to_trim_at_start /
    /// num_samples_to_trim_at_end (for callers whose demuxer trims via
    /// edts/elst).
    pub disable_trim_start: u8,
    pub disable_trim_end: u8,
}

/// Creates a decoder from a descriptor blob (the descriptor OBUs of an IA
/// sequence) and `settings`.
///
/// # Safety
/// `descriptors` must point to `size` readable bytes; `settings` and
/// `out` must be valid pointers.
#[no_mangle]
pub unsafe extern "C" fn iamfrs_decoder_create_from_descriptors(
    descriptors: *const u8,
    size: usize,
    settings: *const IamfrsSettings,
    out: *mut *mut IamfrsDecoder,
) -> c_int {
    if descriptors.is_null() || out.is_null() || settings.is_null() {
        return IAMFRS_ERR_INVALID_ARG;
    }
    let c_settings = unsafe { &*settings };
    let Some(layout) = u8::try_from(c_settings.output_layout)
        .ok()
        .and_then(SoundSystem::from_u8)
    else {
        return IAMFRS_ERR_INVALID_ARG;
    };
    let sample_type = match c_settings.sample_type {
        0 => None,
        1 => Some(OutputSampleType::Int16LittleEndian),
        2 => Some(OutputSampleType::Int32LittleEndian),
        _ => return IAMFRS_ERR_INVALID_ARG,
    };
    let channel_ordering = match c_settings.channel_ordering {
        0 => iamf_dec::stream::ChannelOrdering::Iamf,
        1 => iamf_dec::stream::ChannelOrdering::Android,
        _ => return IAMFRS_ERR_INVALID_ARG,
    };
    let data = unsafe { std::slice::from_raw_parts(descriptors, size) };
    let mix_selection = if c_settings.mix_presentation_id < 0 {
        iamf_dec::stream::MixSelection::Auto
    } else {
        match u32::try_from(c_settings.mix_presentation_id) {
            Ok(id) => iamf_dec::stream::MixSelection::ById(id),
            Err(_) => return IAMFRS_ERR_INVALID_ARG,
        }
    };
    let settings = StreamSettings {
        layout,
        sample_type,
        mix_selection,
        channel_ordering,
        trimming: iamf_dec::stream::TrimmingSettings {
            trim_beginning: c_settings.disable_trim_start == 0,
            trim_end: c_settings.disable_trim_end == 0,
        },
    };
    match StreamDecoder::new_from_descriptors(data, settings, &DefaultFactory) {
        Ok(inner) => {
            let handle = Box::new(IamfrsDecoder {
                inner,
                pending_unit: None,
            });
            unsafe { out.write(Box::into_raw(handle)) };
            IAMFRS_OK
        }
        Err(e) => status_of(&e),
    }
}

/// Pushes bitstream bytes (whole or partial OBUs).
///
/// # Safety
/// `decoder` must be a live handle from create; `data` must point to
/// `size` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn iamfrs_decoder_decode(
    decoder: *mut IamfrsDecoder,
    data: *const u8,
    size: usize,
) -> c_int {
    let Some(handle) = (unsafe { decoder.as_mut() }) else {
        return IAMFRS_ERR_INVALID_ARG;
    };
    if data.is_null() && size != 0 {
        return IAMFRS_ERR_INVALID_ARG;
    }
    let bytes = if size == 0 {
        &[][..]
    } else {
        unsafe { std::slice::from_raw_parts(data, size) }
    };
    match handle.inner.decode(bytes) {
        Ok(()) => IAMFRS_OK,
        Err(e) => status_of(&e),
    }
}

/// Returns 1 when a decoded temporal unit is ready to pull, else 0.
///
/// # Safety
/// `decoder` must be a live handle from create.
#[no_mangle]
pub unsafe extern "C" fn iamfrs_decoder_is_temporal_unit_available(
    decoder: *const IamfrsDecoder,
) -> c_int {
    match unsafe { decoder.as_ref() } {
        Some(h) => (h.pending_unit.is_some() || h.inner.is_temporal_unit_available()) as c_int,
        None => 0,
    }
}

/// Pops one temporal unit as interleaved little-endian PCM into `buffer`.
/// On success `*bytes_written` holds the byte count. When the buffer is
/// too small, returns `IAMFRS_ERR_BUFFER_TOO_SMALL`, sets `*bytes_written`
/// to the required size, and keeps the unit for the next call.
///
/// # Safety
/// `decoder` must be a live handle; `buffer` must point to `capacity`
/// writable bytes (may be null when `capacity` is 0, to query the size);
/// `bytes_written` must be valid and writable.
#[no_mangle]
pub unsafe extern "C" fn iamfrs_decoder_get_output_temporal_unit(
    decoder: *mut IamfrsDecoder,
    buffer: *mut u8,
    capacity: usize,
    bytes_written: *mut usize,
) -> c_int {
    let Some(handle) = (unsafe { decoder.as_mut() }) else {
        return IAMFRS_ERR_INVALID_ARG;
    };
    if bytes_written.is_null() {
        return IAMFRS_ERR_INVALID_ARG;
    }
    if handle.pending_unit.is_none() {
        match handle.inner.get_output_temporal_unit() {
            Ok(Some(unit)) => handle.pending_unit = Some(unit),
            Ok(None) => {
                unsafe { bytes_written.write(0) };
                return IAMFRS_ERR_NO_TEMPORAL_UNIT;
            }
            Err(e) => return status_of(&e),
        }
    }
    let unit = handle.pending_unit.as_ref().expect("filled above");
    unsafe { bytes_written.write(unit.len()) };
    if unit.len() > capacity {
        return IAMFRS_ERR_BUFFER_TOO_SMALL;
    }
    if !unit.is_empty() {
        if buffer.is_null() {
            return IAMFRS_ERR_INVALID_ARG;
        }
        unsafe { std::ptr::copy_nonoverlapping(unit.as_ptr(), buffer, unit.len()) };
    }
    handle.pending_unit = None;
    IAMFRS_OK
}

macro_rules! getter {
    ($name:ident, $ty:ty, $get:expr) => {
        /// # Safety
        /// `decoder` must be a live handle; the out pointer must be valid
        /// and writable.
        #[no_mangle]
        pub unsafe extern "C" fn $name(decoder: *const IamfrsDecoder, out: *mut $ty) -> c_int {
            let Some(handle) = (unsafe { decoder.as_ref() }) else {
                return IAMFRS_ERR_INVALID_ARG;
            };
            if out.is_null() {
                return IAMFRS_ERR_INVALID_ARG;
            }
            #[allow(clippy::redundant_closure_call)]
            unsafe {
                out.write(($get)(&handle.inner))
            };
            IAMFRS_OK
        }
    };
}

getter!(
    iamfrs_decoder_get_num_output_channels,
    u32,
    |d: &StreamDecoder| d.num_output_channels() as u32
);
getter!(iamfrs_decoder_get_sample_rate, u32, |d: &StreamDecoder| d
    .sample_rate());
getter!(iamfrs_decoder_get_frame_size, u32, |d: &StreamDecoder| d
    .frame_size());
getter!(
    iamfrs_decoder_get_selected_mix_presentation_id,
    u32,
    |d: &StreamDecoder| d.selected_mix().0
);
getter!(iamfrs_decoder_get_sample_type, u32, |d: &StreamDecoder| {
    match d.sample_type() {
        OutputSampleType::Int16LittleEndian => 1,
        OutputSampleType::Int32LittleEndian => 2,
    }
});

/// Drops buffered audio and parameter state (seek/discontinuity).
///
/// # Safety
/// `decoder` must be a live handle from create.
#[no_mangle]
pub unsafe extern "C" fn iamfrs_decoder_reset(decoder: *mut IamfrsDecoder) -> c_int {
    let Some(handle) = (unsafe { decoder.as_mut() }) else {
        return IAMFRS_ERR_INVALID_ARG;
    };
    handle.pending_unit = None;
    handle.inner.reset();
    IAMFRS_OK
}

/// Marks end of stream; remaining buffered units stay pullable.
///
/// # Safety
/// `decoder` must be a live handle from create.
#[no_mangle]
pub unsafe extern "C" fn iamfrs_decoder_signal_end_of_decoding(
    decoder: *mut IamfrsDecoder,
) -> c_int {
    let Some(handle) = (unsafe { decoder.as_mut() }) else {
        return IAMFRS_ERR_INVALID_ARG;
    };
    handle.inner.signal_end_of_decoding();
    IAMFRS_OK
}

/// Destroys the decoder. Passing null is a no-op.
///
/// # Safety
/// `decoder` must be null or a live handle from create, and must not be
/// used afterwards.
#[no_mangle]
pub unsafe extern "C" fn iamfrs_decoder_destroy(decoder: *mut IamfrsDecoder) {
    if !decoder.is_null() {
        drop(unsafe { Box::from_raw(decoder) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vector() -> Option<Vec<u8>> {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/vectors/test_000002.iamf");
        std::fs::read(path).ok()
    }

    #[test]
    fn c_api_end_to_end() {
        let Some(data) = vector() else {
            eprintln!("vector missing; run tools/fetch_vectors.sh");
            return;
        };
        let mut decoder: *mut IamfrsDecoder = std::ptr::null_mut();
        // SAFETY: valid pointers throughout; handle lifecycle follows the
        // documented contract.
        unsafe {
            assert_eq!(
                iamfrs_decoder_create_from_descriptors(
                    data.as_ptr(),
                    data.len(),
                    &IamfrsSettings {
                        output_layout: 0,
                        sample_type: 0, // auto: 16-bit LPCM resolves to s16le
                        mix_presentation_id: -1,
                        channel_ordering: 0,
                        disable_trim_start: 0,
                        disable_trim_end: 0,
                    },
                    &mut decoder
                ),
                IAMFRS_OK
            );
            let (mut channels, mut rate, mut frame_size) = (0u32, 0u32, 0u32);
            let (mut mix_id, mut resolved_type) = (0u32, 0u32);
            assert_eq!(
                iamfrs_decoder_get_selected_mix_presentation_id(decoder, &mut mix_id),
                IAMFRS_OK
            );
            assert_eq!(mix_id, 42);
            assert_eq!(
                iamfrs_decoder_get_sample_type(decoder, &mut resolved_type),
                IAMFRS_OK
            );
            assert_eq!(resolved_type, 1);
            assert_eq!(
                iamfrs_decoder_get_num_output_channels(decoder, &mut channels),
                IAMFRS_OK
            );
            assert_eq!(
                iamfrs_decoder_get_sample_rate(decoder, &mut rate),
                IAMFRS_OK
            );
            assert_eq!(
                iamfrs_decoder_get_frame_size(decoder, &mut frame_size),
                IAMFRS_OK
            );
            assert_eq!(channels, 2);
            assert_eq!(rate, 16000);
            assert_eq!(frame_size, 128);

            assert_eq!(
                iamfrs_decoder_decode(decoder, data.as_ptr(), data.len()),
                IAMFRS_OK
            );
            assert_eq!(iamfrs_decoder_signal_end_of_decoding(decoder), IAMFRS_OK);

            let mut total = Vec::new();
            let mut scratch = vec![0u8; 64 * 1024];
            while iamfrs_decoder_is_temporal_unit_available(decoder) == 1 {
                let mut written = 0usize;
                // Size query first: zero capacity must report the size.
                assert_eq!(
                    iamfrs_decoder_get_output_temporal_unit(
                        decoder,
                        std::ptr::null_mut(),
                        0,
                        &mut written
                    ),
                    IAMFRS_ERR_BUFFER_TOO_SMALL
                );
                assert!(written <= scratch.len());
                assert_eq!(
                    iamfrs_decoder_get_output_temporal_unit(
                        decoder,
                        scratch.as_mut_ptr(),
                        scratch.len(),
                        &mut written
                    ),
                    IAMFRS_OK
                );
                total.extend_from_slice(&scratch[..written]);
            }
            iamfrs_decoder_destroy(decoder);
            // 8000 samples x 2 channels x 2 bytes.
            assert_eq!(total.len(), 8000 * 2 * 2);
        }
    }

    #[test]
    fn null_safety() {
        // SAFETY: exercising the documented null-tolerant paths.
        unsafe {
            assert_eq!(
                iamfrs_decoder_decode(std::ptr::null_mut(), std::ptr::null(), 0),
                IAMFRS_ERR_INVALID_ARG
            );
            assert_eq!(
                iamfrs_decoder_is_temporal_unit_available(std::ptr::null()),
                0
            );
            iamfrs_decoder_destroy(std::ptr::null_mut());
        }
    }
}
