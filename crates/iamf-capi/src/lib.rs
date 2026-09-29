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
#![warn(missing_docs)]

use std::ffi::c_int;

use iamf_codecs::DefaultFactory;
use iamf_dec::DecodeError;
use iamf_dec::layout::SoundSystem;
use iamf_dec::stream::{OutputSampleType, StreamDecoder, StreamSettings};

/// Success.
pub const IAMFRS_OK: c_int = 0;
/// A null pointer or out-of-range argument.
pub const IAMFRS_ERR_INVALID_ARG: c_int = -1;
/// The stream needs a codec or profile outside the supported set.
pub const IAMFRS_ERR_UNSUPPORTED: c_int = -2;
/// Malformed descriptors or bitstream data.
pub const IAMFRS_ERR_CORRUPT_DATA: c_int = -3;
/// The output buffer is too small; the required size was reported.
pub const IAMFRS_ERR_BUFFER_TOO_SMALL: c_int = -4;
/// No decoded temporal unit is ready.
pub const IAMFRS_ERR_NO_TEMPORAL_UNIT: c_int = -5;
/// An error that fits no other status.
pub const IAMFRS_ERR_INTERNAL: c_int = -6;

// Values for `IamfrsSettings::output_layout` and the `output_layout`
// argument of `iamfrs_decoder_reset_with_new_mix`, numbered exactly like
// iamf-tools `OutputLayout` (and the IAMF sound_system values 0..=13).

/// ITU-R BS.2051 sound system A (0+2+0), stereo (`kItu2051_SoundSystemA_0_2_0`).
pub const IAMFRS_LAYOUT_SOUND_SYSTEM_A_0_2_0: i32 = 0;
/// Sound system B (0+5+0), 5.1 (`kItu2051_SoundSystemB_0_5_0`).
pub const IAMFRS_LAYOUT_SOUND_SYSTEM_B_0_5_0: i32 = 1;
/// Sound system C (2+5+0), 5.1.2 (`kItu2051_SoundSystemC_2_5_0`).
pub const IAMFRS_LAYOUT_SOUND_SYSTEM_C_2_5_0: i32 = 2;
/// Sound system D (4+5+0), 5.1.4 (`kItu2051_SoundSystemD_4_5_0`).
pub const IAMFRS_LAYOUT_SOUND_SYSTEM_D_4_5_0: i32 = 3;
/// Sound system E (4+5+1) (`kItu2051_SoundSystemE_4_5_1`).
pub const IAMFRS_LAYOUT_SOUND_SYSTEM_E_4_5_1: i32 = 4;
/// Sound system F (3+7+0) (`kItu2051_SoundSystemF_3_7_0`).
pub const IAMFRS_LAYOUT_SOUND_SYSTEM_F_3_7_0: i32 = 5;
/// Sound system G (4+9+0) (`kItu2051_SoundSystemG_4_9_0`).
pub const IAMFRS_LAYOUT_SOUND_SYSTEM_G_4_9_0: i32 = 6;
/// Sound system H (9+10+3), 22.2 (`kItu2051_SoundSystemH_9_10_3`).
pub const IAMFRS_LAYOUT_SOUND_SYSTEM_H_9_10_3: i32 = 7;
/// Sound system I (0+7+0), 7.1 (`kItu2051_SoundSystemI_0_7_0`).
pub const IAMFRS_LAYOUT_SOUND_SYSTEM_I_0_7_0: i32 = 8;
/// Sound system J (4+7+0), 7.1.4 (`kItu2051_SoundSystemJ_4_7_0`).
pub const IAMFRS_LAYOUT_SOUND_SYSTEM_J_4_7_0: i32 = 9;
/// IAMF extension 7.1.2 (`kIAMF_SoundSystemExtension_2_7_0`).
pub const IAMFRS_LAYOUT_EXTENSION_2_7_0: i32 = 10;
/// IAMF extension 3.1.2 (`kIAMF_SoundSystemExtension_2_3_0`).
pub const IAMFRS_LAYOUT_EXTENSION_2_3_0: i32 = 11;
/// IAMF extension mono (`kIAMF_SoundSystemExtension_0_1_0`).
pub const IAMFRS_LAYOUT_EXTENSION_0_1_0: i32 = 12;
/// IAMF extension 9.1.6 (`kIAMF_SoundSystemExtension_6_9_0`).
pub const IAMFRS_LAYOUT_EXTENSION_6_9_0: i32 = 13;
/// Binaural, channels ordered L, R (`kIAMF_Binaural`, iamf-tools v3.0.0): HRTF
/// rendering for elements with `headphones_rendering_mode == 1` (with the
/// `binaural` feature), the stereo gain matrices otherwise.
pub const IAMFRS_LAYOUT_BINAURAL: i32 = 14;

fn status_of(err: &DecodeError) -> c_int {
    match err {
        DecodeError::UnsupportedCodec
        | DecodeError::UnsupportedProfile(_)
        | DecodeError::Unimplemented(_) => IAMFRS_ERR_UNSUPPORTED,
        DecodeError::CorruptPacket(_) | DecodeError::InvalidDescriptors(_) => {
            IAMFRS_ERR_CORRUPT_DATA
        }
        // DecodeError is non-exhaustive; map future variants conservatively.
        _ => IAMFRS_ERR_INTERNAL,
    }
}

/// Opaque decoder handle.
#[derive(Debug)]
pub struct IamfrsDecoder {
    inner: StreamDecoder,
    /// A rendered unit that did not fit the caller's buffer yet.
    pending_unit: Option<Vec<u8>>,
}

/// Decoder configuration, mirroring iamf-tools' `IamfDecoderFactory::Settings`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct IamfrsSettings {
    /// One of the `IAMFRS_LAYOUT_*` constants: IAMF sound system numbering
    /// shared with iamf-tools `OutputLayout` (0 = stereo ... 13 = 9.1.6,
    /// [`IAMFRS_LAYOUT_BINAURAL`] = 14).
    pub output_layout: i32,
    /// 0 = auto (from the stream's bit depth), 1 = s16le, 2 = s32le.
    pub sample_type: i32,
    /// Mix presentation to decode, or -1 to select automatically (a mix
    /// declaring the requested layout, else the first).
    pub mix_presentation_id: i64,
    /// 0 = IAMF rendering order, 1 = Android/WAVE order (iamf-tools
    /// `ChannelOrdering`).
    pub channel_ordering: i32,
    /// Nonzero disables trimming of num_samples_to_trim_at_start
    /// (iamf-tools `TrimmingSettings::trim_beginning = false`), for callers
    /// whose demuxer trims via edts/elst. Zero (trim) is the iamf-tools
    /// default, so zero-initialized settings match it.
    pub disable_trim_start: u8,
    /// Nonzero disables trimming of num_samples_to_trim_at_end (iamf-tools
    /// `TrimmingSettings::trim_end = false`); see `disable_trim_start`.
    pub disable_trim_end: u8,
    /// Bitmask of supported profiles (iamf-tools
    /// `requested_profile_versions`): bit 0 = simple, bit 1 = base,
    /// bit 2 = base-enhanced. 0 means all known profiles.
    pub requested_profiles: u32,
    /// Nonzero enables the libiamf-style -1 dBFS look-ahead peak limiter.
    /// Off by default, matching the iamf-tools decoder.
    pub enable_limiter: u8,
    /// Nonzero enables loudness normalization to `loudness_target_db`
    /// using the stream's loudness_info. Off by default (iamf-tools
    /// ignores loudness metadata).
    pub enable_loudness_normalization: u8,
    /// Target loudness in dB (LKFS), e.g. -24.0; read only when
    /// `enable_loudness_normalization` is nonzero.
    pub loudness_target_db: f32,
}

/// Creates a decoder from a descriptor blob (the descriptor OBUs of an IA
/// sequence) and `settings`.
///
/// # Safety
/// `descriptors` must point to `size` readable bytes; `settings` and
/// `out` must be valid pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iamfrs_decoder_create_from_descriptors(
    descriptors: *const u8,
    size: usize,
    settings: *const IamfrsSettings,
    out: *mut *mut IamfrsDecoder,
) -> c_int {
    if descriptors.is_null() || out.is_null() || settings.is_null() {
        return IAMFRS_ERR_INVALID_ARG;
    }
    // SAFETY: `settings` is non-null (checked) and the caller promises a
    // valid IamfrsSettings.
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
    // SAFETY: `descriptors` is non-null (checked) and the caller promises
    // `size` readable bytes.
    let data = unsafe { std::slice::from_raw_parts(descriptors, size) };
    let mix_selection = if c_settings.mix_presentation_id < 0 {
        iamf_dec::stream::MixSelection::Auto
    } else {
        match u32::try_from(c_settings.mix_presentation_id) {
            Ok(id) => iamf_dec::stream::MixSelection::ById(id),
            Err(_) => return IAMFRS_ERR_INVALID_ARG,
        }
    };
    let mut settings = StreamSettings::default();
    settings.layout = layout;
    settings.sample_type = sample_type;
    settings.mix_selection = mix_selection;
    settings.channel_ordering = channel_ordering;
    settings.trimming.trim_beginning = c_settings.disable_trim_start == 0;
    settings.trimming.trim_end = c_settings.disable_trim_end == 0;
    settings.requested_profiles =
        iamf_dec::profile::ProfileSet::from_bits(c_settings.requested_profiles);
    settings.loudness_target_db =
        (c_settings.enable_loudness_normalization != 0).then_some(c_settings.loudness_target_db);
    settings.enable_limiter = c_settings.enable_limiter != 0;
    match StreamDecoder::new_from_descriptors(data, settings, &DefaultFactory) {
        Ok(inner) => {
            let handle = Box::new(IamfrsDecoder {
                inner,
                pending_unit: None,
            });
            // SAFETY: `out` is non-null (checked) and the caller promises
            // it is writable.
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
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iamfrs_decoder_decode(
    decoder: *mut IamfrsDecoder,
    data: *const u8,
    size: usize,
) -> c_int {
    // SAFETY: the caller promises `decoder` is null or a live handle.
    let Some(handle) = (unsafe { decoder.as_mut() }) else {
        return IAMFRS_ERR_INVALID_ARG;
    };
    if data.is_null() && size != 0 {
        return IAMFRS_ERR_INVALID_ARG;
    }
    let bytes = if size == 0 {
        &[][..]
    } else {
        // SAFETY: `data` is non-null (checked) and the caller promises
        // `size` readable bytes.
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
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iamfrs_decoder_is_temporal_unit_available(
    decoder: *const IamfrsDecoder,
) -> c_int {
    // SAFETY: the caller promises `decoder` is null or a live handle.
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
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iamfrs_decoder_get_output_temporal_unit(
    decoder: *mut IamfrsDecoder,
    buffer: *mut u8,
    capacity: usize,
    bytes_written: *mut usize,
) -> c_int {
    // SAFETY: the caller promises `decoder` is null or a live handle.
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
                // SAFETY: `bytes_written` is non-null (checked) and the
                // caller promises it is writable.
                unsafe { bytes_written.write(0) };
                return IAMFRS_ERR_NO_TEMPORAL_UNIT;
            }
            Err(e) => return status_of(&e),
        }
    }
    let unit = handle.pending_unit.as_ref().expect("filled above");
    // SAFETY: `bytes_written` is non-null (checked) and writable.
    unsafe { bytes_written.write(unit.len()) };
    if unit.len() > capacity {
        return IAMFRS_ERR_BUFFER_TOO_SMALL;
    }
    if !unit.is_empty() {
        if buffer.is_null() {
            return IAMFRS_ERR_INVALID_ARG;
        }
        // SAFETY: `buffer` is non-null (checked), the caller promises
        // `capacity` writable bytes, and unit.len() <= capacity here.
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
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $name(decoder: *const IamfrsDecoder, out: *mut $ty) -> c_int {
            // SAFETY: the caller promises `decoder` is null or a live
            // handle.
            let Some(handle) = (unsafe { decoder.as_ref() }) else {
                return IAMFRS_ERR_INVALID_ARG;
            };
            if out.is_null() {
                return IAMFRS_ERR_INVALID_ARG;
            }
            // SAFETY: `out` is non-null (checked) and the caller promises
            // it is writable.
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
getter!(
    iamfrs_decoder_get_selected_layout,
    u32,
    |d: &StreamDecoder| d.selected_mix().1 as u32
);
getter!(iamfrs_decoder_get_sample_type, u32, |d: &StreamDecoder| {
    match d.sample_type() {
        OutputSampleType::Int16LittleEndian => 1,
        // Int32 today; future formats would need their own constant.
        _ => 2,
    }
});

/// Drops buffered audio and parameter state (seek/discontinuity).
///
/// # Safety
/// `decoder` must be a live handle from create.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iamfrs_decoder_reset(decoder: *mut IamfrsDecoder) -> c_int {
    // SAFETY: the caller promises `decoder` is null or a live handle.
    let Some(handle) = (unsafe { decoder.as_mut() }) else {
        return IAMFRS_ERR_INVALID_ARG;
    };
    handle.pending_unit = None;
    handle.inner.reset();
    IAMFRS_OK
}

/// Reconfigures for a different mix presentation and/or output layout
/// without reparsing descriptors (iamf-tools `ResetWithNewMix`). Codec
/// decoders shared between the old and new mix are reset and reused.
/// `mix_presentation_id < 0` selects automatically; `output_layout < 0`
/// keeps the current layout. Buffered audio and parameter state are
/// dropped. On error the decoder is unconfigured until a successful
/// reconfigure; destroy it or call this again with valid arguments.
///
/// # Safety
/// `decoder` must be a live handle from create.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iamfrs_decoder_reset_with_new_mix(
    decoder: *mut IamfrsDecoder,
    mix_presentation_id: i64,
    output_layout: i32,
) -> c_int {
    // SAFETY: the caller promises `decoder` is null or a live handle.
    let Some(handle) = (unsafe { decoder.as_mut() }) else {
        return IAMFRS_ERR_INVALID_ARG;
    };
    let selection = if mix_presentation_id < 0 {
        iamf_dec::stream::MixSelection::Auto
    } else {
        match u32::try_from(mix_presentation_id) {
            Ok(id) => iamf_dec::stream::MixSelection::ById(id),
            Err(_) => return IAMFRS_ERR_INVALID_ARG,
        }
    };
    let layout = if output_layout < 0 {
        None
    } else {
        match u8::try_from(output_layout)
            .ok()
            .and_then(SoundSystem::from_u8)
        {
            Some(layout) => Some(layout),
            None => return IAMFRS_ERR_INVALID_ARG,
        }
    };
    handle.pending_unit = None;
    match handle
        .inner
        .reset_with_new_mix(selection, layout, &DefaultFactory)
    {
        Ok(_) => IAMFRS_OK,
        Err(e) => status_of(&e),
    }
}

/// Marks end of stream; remaining buffered units stay pullable.
///
/// # Safety
/// `decoder` must be a live handle from create.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iamfrs_decoder_signal_end_of_decoding(
    decoder: *mut IamfrsDecoder,
) -> c_int {
    // SAFETY: the caller promises `decoder` is null or a live handle.
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
#[unsafe(no_mangle)]
pub unsafe extern "C" fn iamfrs_decoder_destroy(decoder: *mut IamfrsDecoder) {
    if !decoder.is_null() {
        // SAFETY: non-null (checked); the caller promises this is a live
        // handle from create, owned solely by us from here.
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
        let data = match vector() {
            Some(data) => data,
            None if std::env::var_os("IAMF_VECTORS_OPTIONAL").is_some() => {
                eprintln!("SKIPPED: test_000002 missing; run tools/fetch_vectors.sh");
                return;
            }
            None => panic!(
                "test_000002 missing; run tools/fetch_vectors.sh \
                 (or set IAMF_VECTORS_OPTIONAL=1 to skip vector tests)"
            ),
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
                        requested_profiles: 0,
                        enable_limiter: 0,
                        enable_loudness_normalization: 0,
                        loudness_target_db: 0.0,
                    },
                    &mut decoder
                ),
                IAMFRS_OK
            );
            // In-place reconfigure (same mix, mono layout) reuses the
            // codec decoders and keeps the handle usable.
            assert_eq!(
                iamfrs_decoder_reset_with_new_mix(decoder, -1, 12),
                IAMFRS_OK
            );
            let mut layout = u32::MAX;
            assert_eq!(
                iamfrs_decoder_get_selected_layout(decoder, &mut layout),
                IAMFRS_OK
            );
            assert_eq!(layout, 12);
            assert_eq!(iamfrs_decoder_reset_with_new_mix(decoder, 42, 0), IAMFRS_OK);
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

    /// The hand-maintained C header must agree with the Rust constants.
    #[test]
    fn header_constants_match_rust() {
        let header = include_str!("../include/iamf_rs.h");
        for (name, value) in [
            ("IAMFRS_OK", IAMFRS_OK),
            ("IAMFRS_ERR_INVALID_ARG", IAMFRS_ERR_INVALID_ARG),
            ("IAMFRS_ERR_UNSUPPORTED", IAMFRS_ERR_UNSUPPORTED),
            ("IAMFRS_ERR_CORRUPT_DATA", IAMFRS_ERR_CORRUPT_DATA),
            ("IAMFRS_ERR_BUFFER_TOO_SMALL", IAMFRS_ERR_BUFFER_TOO_SMALL),
            ("IAMFRS_ERR_NO_TEMPORAL_UNIT", IAMFRS_ERR_NO_TEMPORAL_UNIT),
            ("IAMFRS_ERR_INTERNAL", IAMFRS_ERR_INTERNAL),
        ]
        .into_iter()
        .chain(LAYOUTS)
        {
            let needle = format!("{name} = {value},");
            assert!(header.contains(&needle), "header disagrees on {name}");
        }
        // Sample types, orderings, and profile bits.
        for needle in [
            "IAMFRS_SAMPLE_INT16_LE = 1,",
            "IAMFRS_SAMPLE_INT32_LE = 2,",
            "IAMFRS_ORDERING_IAMF = 0,",
            "IAMFRS_ORDERING_ANDROID = 1,",
            "IAMFRS_PROFILE_SIMPLE = 1 << 0,",
            "IAMFRS_PROFILE_BASE = 1 << 1,",
            "IAMFRS_PROFILE_BASE_ENHANCED = 1 << 2,",
        ] {
            assert!(header.contains(needle), "header missing `{needle}`");
        }
    }

    /// Every `IAMFRS_LAYOUT_*` constant, in iamf-tools `OutputLayout` order.
    const LAYOUTS: [(&str, i32); 15] = [
        (
            "IAMFRS_LAYOUT_SOUND_SYSTEM_A_0_2_0",
            IAMFRS_LAYOUT_SOUND_SYSTEM_A_0_2_0,
        ),
        (
            "IAMFRS_LAYOUT_SOUND_SYSTEM_B_0_5_0",
            IAMFRS_LAYOUT_SOUND_SYSTEM_B_0_5_0,
        ),
        (
            "IAMFRS_LAYOUT_SOUND_SYSTEM_C_2_5_0",
            IAMFRS_LAYOUT_SOUND_SYSTEM_C_2_5_0,
        ),
        (
            "IAMFRS_LAYOUT_SOUND_SYSTEM_D_4_5_0",
            IAMFRS_LAYOUT_SOUND_SYSTEM_D_4_5_0,
        ),
        (
            "IAMFRS_LAYOUT_SOUND_SYSTEM_E_4_5_1",
            IAMFRS_LAYOUT_SOUND_SYSTEM_E_4_5_1,
        ),
        (
            "IAMFRS_LAYOUT_SOUND_SYSTEM_F_3_7_0",
            IAMFRS_LAYOUT_SOUND_SYSTEM_F_3_7_0,
        ),
        (
            "IAMFRS_LAYOUT_SOUND_SYSTEM_G_4_9_0",
            IAMFRS_LAYOUT_SOUND_SYSTEM_G_4_9_0,
        ),
        (
            "IAMFRS_LAYOUT_SOUND_SYSTEM_H_9_10_3",
            IAMFRS_LAYOUT_SOUND_SYSTEM_H_9_10_3,
        ),
        (
            "IAMFRS_LAYOUT_SOUND_SYSTEM_I_0_7_0",
            IAMFRS_LAYOUT_SOUND_SYSTEM_I_0_7_0,
        ),
        (
            "IAMFRS_LAYOUT_SOUND_SYSTEM_J_4_7_0",
            IAMFRS_LAYOUT_SOUND_SYSTEM_J_4_7_0,
        ),
        (
            "IAMFRS_LAYOUT_EXTENSION_2_7_0",
            IAMFRS_LAYOUT_EXTENSION_2_7_0,
        ),
        (
            "IAMFRS_LAYOUT_EXTENSION_2_3_0",
            IAMFRS_LAYOUT_EXTENSION_2_3_0,
        ),
        (
            "IAMFRS_LAYOUT_EXTENSION_0_1_0",
            IAMFRS_LAYOUT_EXTENSION_0_1_0,
        ),
        (
            "IAMFRS_LAYOUT_EXTENSION_6_9_0",
            IAMFRS_LAYOUT_EXTENSION_6_9_0,
        ),
        ("IAMFRS_LAYOUT_BINAURAL", IAMFRS_LAYOUT_BINAURAL),
    ];

    /// The layout constants are exactly the values the ABI accepts, with
    /// binaural last at 14 (iamf-tools `kIAMF_Binaural`).
    #[test]
    fn layout_constants_match_sound_systems() {
        for (index, (name, value)) in LAYOUTS.iter().enumerate() {
            assert_eq!(*value, index as i32, "{name} out of OutputLayout order");
            let sound_system = SoundSystem::from_u8(*value as u8)
                .unwrap_or_else(|| panic!("{name} not accepted by the decoder"));
            assert_eq!(sound_system as i32, *value, "{name} round trip");
        }
        assert_eq!(
            SoundSystem::from_u8(IAMFRS_LAYOUT_BINAURAL as u8),
            Some(SoundSystem::Binaural)
        );
        assert_eq!(SoundSystem::from_u8(LAYOUTS.len() as u8), None);
    }

    /// `iamfrs_settings` is passed by pointer without a size/version field,
    /// so its layout is ABI: a caller built against an older header passes
    /// a smaller struct. Any change here must be deliberate (and versioned).
    #[test]
    fn settings_struct_layout_is_frozen() {
        use std::mem::{align_of, offset_of, size_of};
        assert_eq!(size_of::<IamfrsSettings>(), 40);
        assert_eq!(align_of::<IamfrsSettings>(), 8);
        assert_eq!(offset_of!(IamfrsSettings, output_layout), 0);
        assert_eq!(offset_of!(IamfrsSettings, sample_type), 4);
        assert_eq!(offset_of!(IamfrsSettings, mix_presentation_id), 8);
        assert_eq!(offset_of!(IamfrsSettings, channel_ordering), 16);
        assert_eq!(offset_of!(IamfrsSettings, disable_trim_start), 20);
        assert_eq!(offset_of!(IamfrsSettings, disable_trim_end), 21);
        assert_eq!(offset_of!(IamfrsSettings, requested_profiles), 24);
        assert_eq!(offset_of!(IamfrsSettings, enable_limiter), 28);
        assert_eq!(
            offset_of!(IamfrsSettings, enable_loudness_normalization),
            29
        );
        assert_eq!(offset_of!(IamfrsSettings, loudness_target_db), 32);
    }

    /// Reads a conformance vector; `None` means "skip" (only when
    /// IAMF_VECTORS_OPTIONAL is set), otherwise a missing vector panics.
    fn named_vector(name: &str) -> Option<Vec<u8>> {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../../tests/vectors/{name}.iamf"));
        match std::fs::read(path) {
            Ok(data) => Some(data),
            Err(_) if std::env::var_os("IAMF_VECTORS_OPTIONAL").is_some() => {
                eprintln!("SKIPPED: {name} missing; run tools/fetch_vectors.sh");
                None
            }
            Err(_) => panic!(
                "{name} missing; run tools/fetch_vectors.sh \
                 (or set IAMF_VECTORS_OPTIONAL=1 to skip vector tests)"
            ),
        }
    }

    /// Zero-initialized settings (what a C caller gets from `= {0}`) with
    /// the given layout and automatic mix selection.
    fn settings_for(output_layout: i32) -> IamfrsSettings {
        IamfrsSettings {
            output_layout,
            sample_type: 0,
            mix_presentation_id: -1,
            channel_ordering: 0,
            disable_trim_start: 0,
            disable_trim_end: 0,
            requested_profiles: 0,
            enable_limiter: 0,
            enable_loudness_normalization: 0,
            loudness_target_db: 0.0,
        }
    }

    /// Decodes a whole standalone stream through the C ABI, returning the
    /// PCM bytes, output channel count, and selected layout.
    fn decode_via_c_api(data: &[u8], settings: &IamfrsSettings) -> (Vec<u8>, u32, u32) {
        let mut decoder: *mut IamfrsDecoder = std::ptr::null_mut();
        // SAFETY: valid pointers throughout; the handle is destroyed once.
        unsafe {
            assert_eq!(
                iamfrs_decoder_create_from_descriptors(
                    data.as_ptr(),
                    data.len(),
                    settings,
                    &mut decoder
                ),
                IAMFRS_OK
            );
            let (mut channels, mut layout) = (0u32, u32::MAX);
            assert_eq!(
                iamfrs_decoder_get_num_output_channels(decoder, &mut channels),
                IAMFRS_OK
            );
            assert_eq!(
                iamfrs_decoder_get_selected_layout(decoder, &mut layout),
                IAMFRS_OK
            );
            assert_eq!(
                iamfrs_decoder_decode(decoder, data.as_ptr(), data.len()),
                IAMFRS_OK
            );
            assert_eq!(iamfrs_decoder_signal_end_of_decoding(decoder), IAMFRS_OK);
            let mut pcm = Vec::new();
            let mut scratch = vec![0u8; 1 << 16];
            while iamfrs_decoder_is_temporal_unit_available(decoder) == 1 {
                let mut written = 0usize;
                assert_eq!(
                    iamfrs_decoder_get_output_temporal_unit(
                        decoder,
                        scratch.as_mut_ptr(),
                        scratch.len(),
                        &mut written
                    ),
                    IAMFRS_OK
                );
                pcm.extend_from_slice(&scratch[..written]);
            }
            iamfrs_decoder_destroy(decoder);
            (pcm, channels, layout)
        }
    }

    /// `disable_trim_start` / `disable_trim_end` (iamf-tools
    /// `TrimmingSettings`) independently control the audio frames' trims.
    /// test_000026 is 26 Opus frames of 960 samples, trimming 312 samples
    /// at the start and 648 at the end.
    #[cfg(any(feature = "opus", feature = "opus-ffi"))]
    #[test]
    fn c_api_trimming_settings() {
        let Some(data) = named_vector("test_000026") else {
            return;
        };
        for (disable_start, disable_end, frames) in [
            (0u8, 0u8, 24000usize),
            (1, 0, 24000 + 312),
            (0, 1, 24000 + 648),
            (1, 1, 26 * 960),
        ] {
            let mut settings = settings_for(IAMFRS_LAYOUT_SOUND_SYSTEM_A_0_2_0);
            settings.disable_trim_start = disable_start;
            settings.disable_trim_end = disable_end;
            let (pcm, channels, _) = decode_via_c_api(&data, &settings);
            assert_eq!(channels, 2);
            // Auto sample type resolves to s16le for Opus.
            assert_eq!(
                pcm.len(),
                frames * 2 * 2,
                "disable_trim_start={disable_start} disable_trim_end={disable_end}"
            );
        }
    }

    /// `IAMFRS_LAYOUT_BINAURAL` selects two-channel binaural output, is
    /// reported back by `get_selected_layout`, and is accepted by
    /// `reset_with_new_mix`. test_000002's element uses
    /// headphones_rendering_mode 0, so binaural renders through the stereo
    /// matrices and must match stereo output exactly.
    #[test]
    fn c_api_binaural_layout() {
        let Some(data) = named_vector("test_000002") else {
            return;
        };
        let (binaural, channels, layout) =
            decode_via_c_api(&data, &settings_for(IAMFRS_LAYOUT_BINAURAL));
        assert_eq!(channels, 2);
        assert_eq!(layout, IAMFRS_LAYOUT_BINAURAL as u32);
        let (stereo, _, stereo_layout) =
            decode_via_c_api(&data, &settings_for(IAMFRS_LAYOUT_SOUND_SYSTEM_A_0_2_0));
        assert_eq!(stereo_layout, IAMFRS_LAYOUT_SOUND_SYSTEM_A_0_2_0 as u32);
        assert_eq!(binaural.len(), 8000 * 2 * 2);
        assert!(binaural == stereo, "mode-0 binaural must equal stereo");

        let mut decoder: *mut IamfrsDecoder = std::ptr::null_mut();
        // SAFETY: valid pointers throughout; the handle is destroyed once.
        unsafe {
            assert_eq!(
                iamfrs_decoder_create_from_descriptors(
                    data.as_ptr(),
                    data.len(),
                    &settings_for(IAMFRS_LAYOUT_EXTENSION_0_1_0),
                    &mut decoder
                ),
                IAMFRS_OK
            );
            assert_eq!(
                iamfrs_decoder_reset_with_new_mix(decoder, -1, IAMFRS_LAYOUT_BINAURAL),
                IAMFRS_OK
            );
            let (mut layout, mut channels) = (u32::MAX, 0u32);
            assert_eq!(
                iamfrs_decoder_get_selected_layout(decoder, &mut layout),
                IAMFRS_OK
            );
            assert_eq!(
                iamfrs_decoder_get_num_output_channels(decoder, &mut channels),
                IAMFRS_OK
            );
            assert_eq!((layout, channels), (IAMFRS_LAYOUT_BINAURAL as u32, 2));
            // One past binaural is not a layout.
            assert_eq!(
                iamfrs_decoder_reset_with_new_mix(decoder, -1, IAMFRS_LAYOUT_BINAURAL + 1),
                IAMFRS_ERR_INVALID_ARG
            );
            iamfrs_decoder_destroy(decoder);

            let mut rejected: *mut IamfrsDecoder = std::ptr::null_mut();
            assert_eq!(
                iamfrs_decoder_create_from_descriptors(
                    data.as_ptr(),
                    data.len(),
                    &settings_for(IAMFRS_LAYOUT_BINAURAL + 1),
                    &mut rejected
                ),
                IAMFRS_ERR_INVALID_ARG
            );
            assert!(rejected.is_null());
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
