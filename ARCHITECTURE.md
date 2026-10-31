# Architecture

iamf-rs decodes IAMF (Eclipsa Audio) bitstreams. Its pipeline mirrors the
reference decoders:

```
bytes → OBU parser → descriptors/params → codec decode (per substream)
      → reconstruct (demix / ambisonics / objects) → render (matrices | panner | binaural)
      → mix gains → PCM out
```

This document covers how the crates divide the work, how rendering can be
swapped out, and how correctness is checked.

## Crates and trust boundaries

Untrusted bytes enter through `iamf-obu`. Everything after parsing lives in
`iamf-dec`. Codecs plug in from `iamf-codecs` or from the embedder.

Only three crates use `unsafe`: `iamf-capi`, `iamf-opus-ffi` and
`iamf-obr-wasm`. They sit at FFI (foreign function interface) or WASM
boundaries. Every other crate forbids `unsafe` through the workspace lints.

### `iamf-obu`: parsing

`iamf-obu` parses untrusted input: OBU (open bitstream unit) framing,
descriptors and audio frame payloads.

- It has no dependencies and uses `#![forbid(unsafe_code)]`.
- It checks every list length against the remaining input before it
  allocates.
- The `parse_obu` fuzz target fuzzes it directly.

### `iamf-dec`: decoding, rendering and mixing

`iamf-dec` does everything after parsing. Its main modules:

| Module | What it does |
| --- | --- |
| `element` | Decodes substreams through pluggable codecs. |
| `reconstruct`, `demixer` | Rebuild scalable channel layouts. They keep per-frame state: demixing modes, the w-index and recon gain smoothing. |
| `objects` | Evaluates polar and cartesian object position trajectories, including the ITU-R BS.2127 §10 allocentric sector mapping. Renders objects to loudspeakers with layer-wise 2D VBAP or DBAP panning (vector- or distance-based amplitude panning), as in the OAR `olr` renderer. |
| `render` | Applies gain matrices to reach the output layout. The matrices are in `matrices.rs`, generated from the libiamf / OAR EAR (EBU ADM Renderer) tables. Do not edit that file by hand. |
| `binaural/` | A native port of google/obr, the Open Binaural Renderer. See below. |
| `params` | Evaluates animated gains and positions (`Step`, `Linear`, `Bezier`, `InterLinear`, `InterBezier`). Holds the subblock-accurate parameter timelines (`ParamCursor`). |
| `profile` | Port of the iamf-tools `ProfileFilter`: enforces the requested profiles and per-mix capability limits. |
| `post` | Optional loudness normalization and peak limiter. The streaming decoder applies them when its settings turn them on. Batch callers, such as the `iamfdec` CLI, apply them themselves. |
| `renderer` | The hook for external renderers. See [Renderer backends](#renderer-backends). |
| `presentation` | The batch decoder (`PresentationDecoder`). |
| `stream` | The streaming decoder (`StreamDecoder`). It accepts partial OBUs and can switch mixes in place. This is the main integration surface. |

The binaural renderer works in the SH (spherical harmonics) domain:

1. An SH encoder turns loudspeaker channels and objects into a
   third-order HOA (higher-order ambisonics) bed. Ambisonic input keeps its
   own order.
2. A partitioned FFT convolution applies the embedded SH-HRIR filters
   (head-related impulse responses; `Ambient`, `Direct` or `Reverberant`).
3. A limiter runs on the result.

Both decoders share the same machinery and give byte-identical output. Two
fuzz targets cover them end to end: `decode_stream` and `decode_batch`.

### `iamf-codecs`: codec adapters

`iamf-codecs` implements `SubstreamDecoder` for each codec. Each codec is
behind a feature:

- LPCM: native.
- Opus: the pure-Rust `opus-decoder` crate, or libopus through `opus-ffi`.
- FLAC and AAC-LC: symphonia.

You can also bring your own codecs through the `CodecFactory` trait.

### `iamf-opus-ffi`: libopus bindings

`iamf-opus-ffi` (in `crates/opus-ffi`) is a small safe wrapper over an
external libopus. It links the system libopus or one the embedder provides.
It does not bundle a libopus build.

### `iamf-capi`: C ABI

`iamf-capi` is a C ABI over `stream::StreamDecoder`. It has the same shape
as the iamf-tools decoder API that Chromium uses. For a reference C++
adapter, see `examples/chromium/`. The `capi` fuzz target covers it.

### `iamf-obr-wasm`: binaural renderer for the browser

`iamf-obr-wasm` exposes the binaural renderer from `iamf-dec` as a C ABI.
It builds to a standalone `wasm32-unknown-unknown` module. A WebAudio
AudioWorklet uses it to binauralize the speaker channels of a `<video>` or
`<audio>` element. See [its README](crates/iamf-obr-wasm/README.md).

### `iamf-roar`: ROAR renderer

`iamf-roar` lives outside the workspace and is not published. It renders
through ROAR using the external renderer hook in `iamf-dec`. See
[Renderer backends](#renderer-backends).

## Renderer backends

### Builtin renderer

By default, the streaming decoder renders reconstructed audio elements with
the builtin renderer. That is the gain-matrix renderer, the object panner
and the obr binaural port described above.

This is the conformance path. It is bit-exact against the reference
decoders. The C ABI and the batch `PresentationDecoder` always use it.

### External renderer hook

`iamf_dec::renderer` is a small hook. It replaces only the render step of
`StreamDecoder`. Here is how it works:

- You pass a `MixRendererFactory` to
  `StreamDecoder::new_from_descriptors_with_renderer`.
- For each sub-mix, the factory gets a `MixConfig` and creates a
  `MixRenderer`. The config holds the target layout, sample rate and codec
  frame size, plus one `ElementInfo` per element.
- An `ElementInfo` holds the element ID and its layout: a reconstructed
  loudspeaker layout, an ambisonics order, or an object count. It also
  holds `headphones_rendering_mode`, the binaural filter profile and the
  default demixing mode.
- For each temporal unit, the renderer gets one `ElementFrame` per element
  and returns the mixed output planes. An `ElementFrame` holds the planes
  with the element mix gain already applied, the unit's demixing mode, and
  object positions for object-based elements.

Everything else stays in the decoder: parsing, codec decoding,
reconstruction (demixing, recon gain, ambisonics), parameter timelines, the
output mix gain, trimming, loudness normalization, the limiter, quantization
and channel ordering.

The decoder creates the renderer at the first unit with audio, because only
then is the output sample rate known. It creates it again after a reset or
a mix change. `StreamSettings` stays `Copy`, because the factory is a
constructor argument rather than a setting.

Layer selection for binaural output differs between the two backends:

- For `headphones_rendering_mode` 1 or 2, an external renderer always gets
  the element's highest layer. It binauralizes both modes itself.
- The builtin renderer does the same for modes 1 and 2, but only with the
  `binaural` feature. Without it, the builtin renderer renders stereo.

### ROAR adapter (`crates/iamf-roar`)

`crates/iamf-roar` implements the hook with AOM's
[ROAR](https://github.com/AOMediaCodec/roar). ROAR is the Rust port of OAR,
the Open Audio Renderer that IAMF v2 references.

ROAR is a git dependency pinned to a commit, and it needs Rust 1.97.1. So
the crate is excluded from the workspace and has its own `Cargo.lock`.
Nothing in the workspace refers to ROAR. This keeps offline builds and
`cargo publish` working, and keeps the workspace MSRV at 1.85. The
repository's pinned toolchain is 1.97.1, so the crate builds with plain
`cargo`. CI builds, lints and tests it in a separate job.

The adapter maps one sub-mix to one `RoarRenderer`:

- The sub-mix becomes a ROAR audio group. Each audio element becomes a ROAR
  element: a loudspeaker layout, an ambisonics order, or a set of objects.
- Object positions are forwarded to ROAR for each unit.
- Binaural-input elements become stereo elements rendered
  world-locked-restricted, so they pass through unchanged.
- The element mix gain (animated, subblock-accurate) arrives already applied
  to the element's planes. Both renderers are linear, so the result is the
  same. This keeps a single implementation of IAMF parameter timing.
- Elements that carry a demixing parameter get ROAR's IAMF downmix renderer
  (`DownmixInfo`). Per-frame `dmixp_mode` updates are forwarded. ROAR does
  not expose the default `w_idx` input, so the adapter passes `None`.
- Binaural targets map `headphones_rendering_mode` 0, 1 and 2 to ROAR's
  world-locked-restricted (stereo), world-locked and head-locked modes.
- Frames are rendered in blocks of the codec frame size, or its largest
  divisor up to 16384. A short final block is zero-padded.

The crate also has `iamfdec-roar`, a CLI that decodes a file to WAV through
ROAR. It takes the same decode options as `iamfdec`. Its loudness
normalization and limiter run in the decoder, before quantization.

### How ROAR compares to the builtin renderer

These numbers are measured on the curated conformance vectors, ROAR against
builtin, in `crates/iamf-roar/tests/roar.rs`. The test pins the loudspeaker
results. The binaural rows come from a smoke test that only prints the
SNR (signal-to-noise ratio).

| Case | Agreement |
| --- | --- |
| Channel-based and ambisonics to all 15 loudspeaker sound systems | Bit-exact or ≥ 141 dB SNR (max diff ≤ 1.2e-7). ROAR's EAR path uses the same libiamf tables. |
| 7.1.4 with demixing info to 5.1.2, 5.1.4, 7.1.2 or 3.1.2 (vectors 000070, 000082) | 6.6–88 dB. A design difference: ROAR applies OAR's IAMF downmix renderer, while the builtin renders the reconstructed layout through the static matrix. |
| Ambisonics to sound system H (22.2) | Refused with `DecodeError::UnsupportedRenderer`, because of an upstream ROAR bug (see below). |
| Binaural, mode 0 | Identical where both render stereo. Ambisonics inputs differ, because ROAR always uses OBR for scene-based elements. |
| Binaural, modes 1 and 2 | 37–90 dB. Both are independent ports of obr, so they are not bit-identical. Without head tracking, each renderer gives the same output for both modes. |

### Known upstream ROAR bug

This bug exists at the pinned ROAR commit. It is in
`src/renderer/ear/ambisonic_to_channel_renderer.rs`, in
`element_renderer_render_h2m`:

- The dual-LFE re-insertion compares the LFE index against the *source*
  channel index, instead of the destination index.
- Sound system H has LFEs at channels 3 and 9. So the channel that belongs
  in output 10 (SiL) is written into LFE2 and then zeroed. Output 10 keeps a
  copy of output 12 (TpFL).
- Other layouts have at most one LFE before the last non-LFE channel, so
  they are not affected.

Until the pinned revision has a fix, `iamf-roar` refuses scene-based
elements to sound system H. The test checks for the refusal.

iamf-tools does not publish its OAR integration. `OarLayoutRenderer`
appears in its history but is not exported. So this adapter was designed
independently against ROAR's public API.

## Correctness strategy

Every stage is checked against the reference implementations on the libiamf
test vector suite (`tools/fetch_vectors.sh`):

- Lossless and same-layout paths must be bit-exact.
- Lossy and cross-implementation paths must stay within a few LSB (least
  significant bits).
- Binaural output is compared with reference renders made through obr.
- The streaming decoder must match the batch decoder byte for byte, however
  the input is split into chunks.

Sometimes the references disagree with each other or with the spec. Then we
match iamf-tools, because Chromium ships it, and we file an issue with the
`upstream` label.

## Generated files

Some files are generated by `tools/extract_*.py` from upstream sources:

- `crates/iamf-dec/src/matrices.rs` (from `tools/extract_matrices.py`)
- `crates/iamf-dec/assets/binaural/*.wav` (from
  `tools/extract_binaural_filters.py`)

Regenerate these files instead of editing them by hand. Licensing and
derivation notes are in `NOTICE`.
