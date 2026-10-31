# iamf-obr-wasm

`iamf-obr-wasm` lets you run the iamf-rs binaural renderer in a web page. It
wraps the pure-Rust port of the
[Open Binaural Renderer (OBR)](https://github.com/google/obr) from
`iamf-dec` in a small C ABI, and compiles it to WebAssembly.

The result is `obr_binaural.wasm`, a standalone `wasm32-unknown-unknown`
module with no imports. It runs synchronously inside a WebAudio
`AudioWorkletProcessor` ([`docs/obr-worklet.js`](../../docs/obr-worklet.js)).

## How it works

1. A `<video>` or `<audio>` element decodes multichannel IAMF, and a
   `MediaElementAudioSourceNode` feeds its channels to the worklet.
2. The renderer encodes the speaker channels into third-order SH
   (spherical harmonics, ACN/SN3D).
3. It convolves them with OBR's SH-HRIR filters (head-related impulse
   responses) and outputs 2-channel binaural audio in real time.

You can pick one of three filter profiles: `Ambient`, `Direct` or
`Reverberant`.

`obr_renderer_create_for_channels` picks the speaker layout from the
channel count:

| Channels | Layout |
| ---: | --- |
| 1 | Mono |
| 2 | Stereo |
| 6 | 5.1 |
| 8 | 7.1 |
| 10 | 5.1.4 |
| 12 | 7.1.4 |
| 16 | 9.1.6 |
| 17 | 7.1.5.4 |
| 24 | 22.2 (10.2.9.3) |

Chromium's `IamfAudioDecoder` outputs channels in Android order, which swaps
the side and rear surround pairs of 7.1, 7.1.2 and 7.1.4. Pass a nonzero
`android_channel_order` and the renderer swaps them back.

## Build for WebAssembly

You need the `wasm32-unknown-unknown` target for the pinned toolchain
(`rustup target add wasm32-unknown-unknown`). Then, from the repository
root:

```sh
RUSTFLAGS="-C opt-level=s -C strip=symbols" \
  cargo build -p iamf-obr-wasm --target wasm32-unknown-unknown --release
cp target/wasm32-unknown-unknown/release/iamf_obr_wasm.wasm docs/obr_binaural.wasm
```

## Exported C and WASM API

Memory helpers:

- `obr_wasm_alloc_f32(len: usize) -> *mut f32`
- `obr_wasm_free_f32(ptr: *mut f32, len: usize)`

Create a renderer. `profile` is `0` (Ambient), `1` (Direct) or
`2` (Reverberant). The create functions return null on bad arguments.

- `obr_renderer_create_for_channels(num_channels: usize, profile: u8, frame_size: usize, sample_rate: u32, android_channel_order: u8) -> *mut ObrWasmRenderer`
- `obr_renderer_create_speakers(loudspeaker_layout: u8, profile: u8, frame_size: usize, sample_rate: u32, android_channel_order: u8) -> *mut ObrWasmRenderer`
- `obr_renderer_create_hoa(order: usize, profile: u8, frame_size: usize, sample_rate: u32) -> *mut ObrWasmRenderer`

`obr_renderer_create_speakers` takes an IAMF `loudspeaker_layout` (`0..=8`),
or an expanded parent layout ID (`16..=18`). `obr_renderer_create_hoa`
takes an ambisonics order from `0` to `4`.

Use and destroy a renderer:

- `obr_renderer_input_channels(renderer: *const ObrWasmRenderer) -> usize`
- `obr_renderer_process_planar(renderer: *mut ObrWasmRenderer, input_planar: *const f32, num_channels: usize, frame_size: usize, out_left: *mut f32, out_right: *mut f32) -> i32`
- `obr_renderer_destroy(renderer: *mut ObrWasmRenderer)`

`obr_renderer_process_planar` reads `num_channels` planes of `frame_size`
samples each, one after another. `frame_size` must match the value you
passed at creation. It returns `0` on success, or a negative error code.

## Live demo

Open <https://baylesj.github.io/iamf-rs/>. The source is in
[`docs/index.html`](../../docs/index.html) and
[`docs/obr-worklet.js`](../../docs/obr-worklet.js).
