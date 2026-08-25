//! Minimal safe decoder wrapper over an external libopus. Only the
//! decode-side entry points IAMF needs; the library is provided by the
//! system or the integrator (see build.rs), never bundled.

#![deny(unsafe_op_in_unsafe_fn)]

use std::ffi::{c_int, c_uchar};
use std::ptr::NonNull;

#[repr(C)]
struct OpusDecoderState {
    _private: [u8; 0],
}

extern "C" {
    fn opus_decoder_create(fs: i32, channels: c_int, error: *mut c_int) -> *mut OpusDecoderState;
    fn opus_decoder_destroy(st: *mut OpusDecoderState);
    fn opus_decode_float(
        st: *mut OpusDecoderState,
        data: *const c_uchar,
        len: i32,
        pcm: *mut f32,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
}

pub struct Decoder {
    state: NonNull<OpusDecoderState>,
    channels: usize,
}

// libopus decoder state is plain heap data with no thread affinity; it is
// safe to move across threads (but not to share: no Sync).
unsafe impl Send for Decoder {}

#[derive(Debug)]
pub struct OpusError(pub i32);

impl std::fmt::Display for OpusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "libopus error {}", self.0)
    }
}

impl std::error::Error for OpusError {}

impl Decoder {
    /// `sample_rate` must be 8/12/16/24/48 kHz; `channels` 1 or 2.
    pub fn new(sample_rate: u32, channels: usize) -> Result<Self, OpusError> {
        let mut error: c_int = 0;
        // SAFETY: valid out-pointer; arguments validated by libopus, which
        // reports failure through `error`.
        let state =
            unsafe { opus_decoder_create(sample_rate as i32, channels as c_int, &mut error) };
        match NonNull::new(state) {
            Some(state) if error == 0 => Ok(Decoder { state, channels }),
            _ => Err(OpusError(error)),
        }
    }

    /// Decodes one packet into interleaved f32; returns samples per
    /// channel. `pcm` must hold at least `channels * max_frame_samples`.
    pub fn decode_float(&mut self, packet: &[u8], pcm: &mut [f32]) -> Result<usize, OpusError> {
        let frame_capacity = (pcm.len() / self.channels.max(1)) as c_int;
        let len = i32::try_from(packet.len()).map_err(|_| OpusError(-1))?;
        // SAFETY: `packet` and `pcm` are valid for their stated lengths;
        // frame_capacity bounds what libopus may write.
        let ret = unsafe {
            opus_decode_float(
                self.state.as_ptr(),
                packet.as_ptr(),
                len,
                pcm.as_mut_ptr(),
                frame_capacity,
                0,
            )
        };
        if ret < 0 {
            Err(OpusError(ret))
        } else {
            Ok(ret as usize)
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: `state` came from opus_decoder_create and is dropped
        // exactly once.
        unsafe { opus_decoder_destroy(self.state.as_ptr()) };
    }
}
