# iamf-obr-wasm

WebAssembly and C ABI bindings for `iamf-dec`'s pure-Rust port of the [Open Binaural Renderer (OBR)](https://github.com/google/obr).

`iamf-obr-wasm` compiles to a standalone, zero-import `wasm32-unknown-unknown` module (`obr_binaural.wasm`) that runs synchronously inside a WebAudio `AudioWorkletProcessor` (`docs/obr-worklet.js`). When a `<video>` or `<audio>` element decoding N-channel IAMF (`7.1.4`, `5.1.4`, `7.1`, `5.1`, or `2.0`) is connected to a `MediaElementAudioSourceNode`, the worklet encodes the discrete speaker channels into 3rd-order Spherical Harmonics (`ACN/SN3D`) and convolves them with OBR's SH-HRIR filters (`Ambient`, `Direct`, or `Reverberant`) to output 2-channel binaural audio in real time.

## Build for WebAssembly

```sh
RUSTFLAGS="-C opt-level=s -C strip=symbols" \
  cargo build -p iamf-obr-wasm --target wasm32-unknown-unknown --release
cp target/wasm32-unknown-unknown/release/iamf_obr_wasm.wasm docs/obr_binaural.wasm
```

## Exported C / WASM API

- `obr_wasm_alloc_f32(len: usize) -> *mut f32`
- `obr_wasm_free_f32(ptr: *mut f32, len: usize)`
- `obr_renderer_create_for_channels(num_channels: usize, profile: u8, frame_size: usize, sample_rate: u32, android_channel_order: u8) -> *mut ObrWasmRenderer`
- `obr_renderer_create_speakers(loudspeaker_layout: u8, profile: u8, frame_size: usize, sample_rate: u32, android_channel_order: u8) -> *mut ObrWasmRenderer`
- `obr_renderer_create_hoa(order: usize, profile: u8, frame_size: usize, sample_rate: u32) -> *mut ObrWasmRenderer`
- `obr_renderer_input_channels(renderer: *const ObrWasmRenderer) -> usize`
- `obr_renderer_process_planar(renderer: *mut ObrWasmRenderer, input_planar: *const f32, num_channels: usize, frame_size: usize, out_left: *mut f32, out_right: *mut f32) -> i32`
- `obr_renderer_destroy(renderer: *mut ObrWasmRenderer)`

## Live Demo

See [`docs/index.html`](../../docs/index.html) and [`docs/obr-worklet.js`](../../docs/obr-worklet.js), or open <https://baylesj.github.io/iamf-rs/>.
