//! WebAssembly and C ABI bindings for [`iamf_dec::binaural::BinauralRenderer`]
//! (pure-Rust port of Google's Open Binaural Renderer, <https://github.com/google/obr>).
//!
//! Designed for real-time WebAudio `AudioWorkletProcessor` streaming when a
//! `<video>` or `<audio>` element decoding N-channel IAMF (`7.1.4`, `5.1.4`,
//! `7.1`, `5.1`, etc.) is connected to a `MediaElementAudioSourceNode`.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

use std::slice;

use iamf_dec::binaural::{BinauralInput, BinauralRenderer, FilterProfile};
use iamf_dec::layout::{
    LOUDSPEAKER_LAYOUT_7_1_5_4, LOUDSPEAKER_LAYOUT_9_1_6, LOUDSPEAKER_LAYOUT_10_2_9_3,
};

/// Opaque handle to a stateful OBR binaural renderer instance.
#[derive(Debug)]
pub struct ObrWasmRenderer {
    inner: BinauralRenderer,
    frame_size: usize,
    loudspeaker_layout: Option<u8>,
    android_channel_order: bool,
    scratch_planes: Vec<Vec<f32>>,
}

fn parse_profile(profile: u8) -> FilterProfile {
    match profile {
        1 => FilterProfile::Direct,
        2 => FilterProfile::Reverberant,
        _ => FilterProfile::Ambient,
    }
}

/// Maps a discrete channel count (e.g. from `MediaElementAudioSourceNode`) to
/// its standard IAMF loudspeaker layout ID.
pub fn layout_for_channel_count(num_channels: usize) -> Option<u8> {
    Some(match num_channels {
        1 => 0,                            // Mono
        2 => 1,                            // Stereo (2.0)
        6 => 2,                            // 5.1
        8 => 5,                            // 7.1
        10 => 4,                           // 5.1.4
        12 => 7,                           // 7.1.4
        16 => LOUDSPEAKER_LAYOUT_9_1_6,    // 9.1.6
        17 => LOUDSPEAKER_LAYOUT_7_1_5_4,  // 7.1.5.4
        24 => LOUDSPEAKER_LAYOUT_10_2_9_3, // 22.2 (10.2.9.3)
        _ => return None,
    })
}

/// Allocates a buffer of `len` `f32` values in WASM linear memory.
#[unsafe(no_mangle)]
pub extern "C" fn obr_wasm_alloc_f32(len: usize) -> *mut f32 {
    if len == 0 {
        return ptr::null_mut();
    }
    let mut buf = vec![0.0f32; len].into_boxed_slice();
    let ptr = buf.as_mut_ptr();
    core::mem::forget(buf);
    ptr
}

/// Frees a buffer previously allocated by [`obr_wasm_alloc_f32`].
///
/// # Safety
/// `ptr` must have been returned by [`obr_wasm_alloc_f32`] with the same `len`
/// and not yet freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn obr_wasm_free_f32(ptr: *mut f32, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    // SAFETY: Caller guarantees `ptr` and `len` came from `obr_wasm_alloc_f32`.
    unsafe {
        let slice = ptr::slice_from_raw_parts_mut(ptr, len);
        drop(Box::from_raw(slice));
    }
}

use core::ptr;

/// Creates an OBR binaural renderer for an IAMF `loudspeaker_layout` (`0..=8`
/// or expanded parent `16..=18`).
///
/// - `profile`: `0` = Ambient, `1` = Direct, `2` = Reverberant.
/// - `frame_size`: samples per channel per call (e.g. `128` for WebAudio `AudioWorklet`).
/// - `sample_rate`: output sample rate in Hz (e.g. `48000`).
/// - `android_channel_order`: nonzero when the input channels arrive in
///   Android / Windows / Chromium `ChannelOrdering::kOrderingForAndroid` order
///   (where 7.1 / 7.1.2 / 7.1.4 place Rear Surround `Lrs, Rrs` at channels 4,5
///   and Side Surround `Lss, Rss` at channels 6,7).
#[unsafe(no_mangle)]
pub extern "C" fn obr_renderer_create_speakers(
    loudspeaker_layout: u8,
    profile: u8,
    frame_size: usize,
    sample_rate: u32,
    android_channel_order: u8,
) -> *mut ObrWasmRenderer {
    let Ok(inner) = BinauralRenderer::with_profile(
        BinauralInput::Speakers { loudspeaker_layout },
        parse_profile(profile),
        frame_size,
        sample_rate,
    ) else {
        return ptr::null_mut();
    };
    let input_channels = inner.input_channels();
    let renderer = ObrWasmRenderer {
        inner,
        frame_size,
        loudspeaker_layout: Some(loudspeaker_layout),
        android_channel_order: android_channel_order != 0,
        scratch_planes: vec![vec![0.0f32; frame_size]; input_channels],
    };
    Box::into_raw(Box::new(renderer))
}

/// Creates an OBR binaural renderer sized automatically from a discrete
/// speaker channel count (`1`, `2`, `6`, `8`, `10`, `12`, `16`, `17`, or `24`).
#[unsafe(no_mangle)]
pub extern "C" fn obr_renderer_create_for_channels(
    num_channels: usize,
    profile: u8,
    frame_size: usize,
    sample_rate: u32,
    android_channel_order: u8,
) -> *mut ObrWasmRenderer {
    let Some(layout) = layout_for_channel_count(num_channels) else {
        return ptr::null_mut();
    };
    obr_renderer_create_speakers(
        layout,
        profile,
        frame_size,
        sample_rate,
        android_channel_order,
    )
}

/// Creates an OBR binaural renderer for ACN/SN3D Higher-Order Ambisonics
/// (`order` `0..=4`, where channel count is `(order + 1)^2`).
#[unsafe(no_mangle)]
pub extern "C" fn obr_renderer_create_hoa(
    order: usize,
    profile: u8,
    frame_size: usize,
    sample_rate: u32,
) -> *mut ObrWasmRenderer {
    let Ok(inner) = BinauralRenderer::with_profile(
        BinauralInput::Hoa { order },
        parse_profile(profile),
        frame_size,
        sample_rate,
    ) else {
        return ptr::null_mut();
    };
    let input_channels = inner.input_channels();
    let renderer = ObrWasmRenderer {
        inner,
        frame_size,
        loudspeaker_layout: None,
        android_channel_order: false,
        scratch_planes: vec![vec![0.0f32; frame_size]; input_channels],
    };
    Box::into_raw(Box::new(renderer))
}

/// Returns the number of input channels expected by `renderer`, or `0` if null.
///
/// # Safety
/// `renderer` must be null or a valid pointer returned by `obr_renderer_create_*`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn obr_renderer_input_channels(renderer: *const ObrWasmRenderer) -> usize {
    if renderer.is_null() {
        return 0;
    }
    // SAFETY: Caller guarantees `renderer` is valid.
    unsafe { (*renderer).inner.input_channels() }
}

/// Renders one block of `frame_size` planar `f32` samples across `num_channels`
/// into binaural `out_left` and `out_right` buffers (each of length `frame_size`).
///
/// `input_planar` is laid out as `num_channels` contiguous planes of
/// `frame_size` floats each: `input_planar[ch * frame_size .. (ch + 1) * frame_size]`.
///
/// Returns `0` on success or a negative error code on invalid arguments.
///
/// # Safety
/// - `renderer` must be a valid pointer returned by `obr_renderer_create_*`.
/// - `input_planar` must point to at least `num_channels * frame_size` valid `f32`s.
/// - `out_left` and `out_right` must each point to at least `frame_size` writable `f32`s.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn obr_renderer_process_planar(
    renderer: *mut ObrWasmRenderer,
    input_planar: *const f32,
    num_channels: usize,
    frame_size: usize,
    out_left: *mut f32,
    out_right: *mut f32,
) -> i32 {
    if renderer.is_null() || input_planar.is_null() || out_left.is_null() || out_right.is_null() {
        return -1;
    }
    // SAFETY: Caller guarantees `renderer` is a valid exclusive pointer.
    let renderer = unsafe { &mut *renderer };
    let expected_channels = renderer.inner.input_channels();
    if frame_size != renderer.frame_size || num_channels < expected_channels {
        return -2;
    }

    // SAFETY: Caller guarantees `input_planar` has `num_channels * frame_size` floats.
    let input = unsafe { slice::from_raw_parts(input_planar, num_channels * frame_size) };
    for ch in 0..expected_channels {
        let src = &input[ch * frame_size..(ch + 1) * frame_size];
        renderer.scratch_planes[ch].copy_from_slice(src);
    }

    // Chromium's IamfAudioDecoder configures `ChannelOrdering::kOrderingForAndroid`,
    // which swaps Rear Surround (Lrs/Rrs) and Side Surround (Lss/Rss) on 7.1,
    // 7.1.2, and 7.1.4 layouts. Swap them back to IAMF/OBR rendering order
    // `[L, R, C, LFE, Lss, Rss, Lrs, Rrs, ...]` before SH encoding.
    if renderer.android_channel_order
        && matches!(renderer.loudspeaker_layout, Some(5..=7))
        && expected_channels >= 8
    {
        renderer.scratch_planes.swap(4, 6);
        renderer.scratch_planes.swap(5, 7);
    }

    let Ok([left, right]) = renderer.inner.process(&renderer.scratch_planes) else {
        return -3;
    };

    // SAFETY: Caller guarantees `out_left` and `out_right` hold `frame_size` floats.
    unsafe {
        slice::from_raw_parts_mut(out_left, frame_size).copy_from_slice(&left[..frame_size]);
        slice::from_raw_parts_mut(out_right, frame_size).copy_from_slice(&right[..frame_size]);
    }
    0
}

/// Destroys a renderer instance created by `obr_renderer_create_*`.
///
/// # Safety
/// `renderer` must be null or a pointer returned by `obr_renderer_create_*`
/// that has not yet been destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn obr_renderer_destroy(renderer: *mut ObrWasmRenderer) {
    if renderer.is_null() {
        return;
    }
    // SAFETY: Caller guarantees `renderer` was allocated by `Box::into_raw`.
    unsafe {
        drop(Box::from_raw(renderer));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: a sample rate of 0 divided by zero in the HRIR
    /// resampler, aborting the process (a trap in WASM). Unsupported
    /// rates now return null.
    #[test]
    fn unsupported_sample_rates_return_null() {
        for rate in [0, 4_800_000] {
            assert!(obr_renderer_create_for_channels(12, 0, 128, rate, 1).is_null());
            assert!(obr_renderer_create_hoa(3, 0, 128, rate).is_null());
        }
        let renderer = obr_renderer_create_for_channels(12, 0, 128, 44_100, 1);
        assert!(!renderer.is_null());
        // SAFETY: created above and destroyed once.
        unsafe { obr_renderer_destroy(renderer) };
    }

    #[test]
    fn renders_7_1_4_block_at_128_frames() {
        let renderer = obr_renderer_create_for_channels(12, 0, 128, 48_000, 1);
        assert!(!renderer.is_null());
        let mut input = vec![0.0f32; 12 * 128];
        // Put a 375 Hz sine wave on Left Top Front (channel 8).
        for i in 0..128 {
            input[8 * 128 + i] = (i as f32 * 0.05).sin() * 0.5;
        }
        let mut left = vec![0.0f32; 128];
        let mut right = vec![0.0f32; 128];
        // SAFETY: All pointers and lengths are valid and match the renderer config.
        unsafe {
            assert_eq!(obr_renderer_input_channels(renderer), 12);
            let rc = obr_renderer_process_planar(
                renderer,
                input.as_ptr(),
                12,
                128,
                left.as_mut_ptr(),
                right.as_mut_ptr(),
            );
            assert_eq!(rc, 0);
            obr_renderer_destroy(renderer);
        }
        let energy_l: f32 = left.iter().map(|x| x * x).sum();
        let energy_r: f32 = right.iter().map(|x| x * x).sum();
        assert!(energy_l > 1e-4);
        assert!(energy_r > 1e-5);
        // Left Top Front should produce stronger energy in the left ear than right.
        assert!(energy_l > energy_r);
    }
}
