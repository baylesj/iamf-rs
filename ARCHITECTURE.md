# Architecture

iamf-rs decodes IAMF (Eclipsa Audio) bitstreams. The pipeline mirrors the
reference decoders:

```
bytes → OBU parser → descriptors/params → codec decode (per substream)
      → reconstruct (demix / ambisonics) → render (matrices | binaural)
      → mix gains → PCM out
```

## Crates and trust boundaries

- **`iamf-obu`** — parsing of untrusted input: OBU framing, descriptor and
  audio-frame payloads. Zero dependencies, `#![forbid(unsafe_code)]`,
  every list length validated against remaining input before allocation.
  Fuzzed directly (`fuzz/parse_obu`).
- **`iamf-dec`** — everything after parsing. `element` decodes substreams
  via pluggable codecs; `reconstruct`/`demixer` rebuild scalable channel
  layouts (per-frame state: demix modes, w-index, recon-gain smoothing);
  `objects` evaluates polar/cartesian object position trajectories
  (including ITU-R BS.2127 §10 allocentric sector mapping) and renders
  objects to loudspeakers via layer-wise 2D VBAP / DBAP panning (OAR
  `olr`); `render` applies gain matrices extracted from libiamf v2.0.0
  (`matrices.rs`, generated — do not edit); `binaural/` is a native port
  of google/obr (SH encoder → HOA bed → partitioned FFT convolution with
  embedded SH-HRIR filters (`Ambient`, `Direct`, `Reverberant`) →
  limiter); `params` evaluates animated gains and positions (`Step`,
  `Linear`, `Bezier`, `InterLinear`, `InterBezier`) and carries the
  subblock-granular parameter timelines (`ParamCursor`);
  `profile` ports iamf-tools' `ProfileFilter` (requested-profile
  enforcement, per-mix capability limits); `post` holds the optional
  loudness/limiter stage — the streaming driver applies it when its
  settings enable it; batch callers (the CLI) apply it themselves. Two
  drivers share this machinery: `presentation` (batch) and `stream`
  (incremental, partial-OBU input — the integration surface, including
  in-place mix switching). Fuzzed end-to-end (`fuzz/decode_stream`).
- **`iamf-codecs`** — `SubstreamDecoder` adapters, all feature-gated:
  LPCM (native), Opus (pure-Rust `opus-decoder`, or libopus via
  `opus-ffi`), FLAC and AAC-LC (symphonia). Integrators can inject their
  own codecs through the `CodecFactory` trait instead.
- **`iamf-capi`** — the only crate with `unsafe` (FFI boundary): a C ABI
  over `stream::StreamDecoder`, shaped after the iamf-tools decoder API
  Chromium consumes. See `examples/chromium/` for a reference C++
  adapter.
- **`iamf-roar`** (outside the workspace, unpublished) — renders through
  ROAR via iamf-dec's external renderer hook; see "Renderer backends".

## Renderer backends

The streaming decoder renders reconstructed audio elements with the
builtin renderer: the libiamf-v1.1-matrix renderer and the obr binaural
port described above. This is the conformance path. It is bit-exact
against the reference decoders, and the C ABI and the batch
`PresentationDecoder` always use it.

`iamf-dec::renderer` is a small hook that replaces only the render step
of `StreamDecoder`. A `MixRendererFactory` passed to
`StreamDecoder::new_from_descriptors_with_renderer` gets a `MixConfig`
(target layout, sample rate, codec frame size, and one `ElementInfo` per
element: id, reconstructed loudspeaker layout or ambisonics order,
`headphones_rendering_mode`, default demixing mode) and creates a
`MixRenderer`. Per temporal unit, the renderer gets one `ElementFrame` per
element (planes with the element mix gain applied, plus the unit's
demixing mode) and returns the mixed output planes. Parsing, codec
decode, reconstruction (demixing, recon gain, ambisonics), parameter
timelines, the output-mix gain, trimming, loudness normalization, the
limiter and quantization stay in the decoder. The renderer is created at
the first unit with audio (the output sample rate is known only then)
and again after a reset or mix change. For binaural output with
`headphones_rendering_mode` 1 or 2, an external renderer always gets the
element's highest layer, since it binauralizes both modes itself; the
builtin renderer does that only for mode 1 with the `binaural` feature.
`StreamSettings` stays `Copy`; the factory is a constructor argument.

`crates/iamf-roar` implements the hook with AOM's
[ROAR](https://github.com/AOMediaCodec/roar), the Rust port of the Open
Audio Renderer that IAMF v2 references. ROAR is a git dependency pinned
to a commit and needs Rust 1.97.1, so the crate is excluded from the
workspace and has its own `Cargo.lock`. Nothing in the workspace refers
to ROAR, which keeps offline builds and `cargo publish` working and the
1.85 MSRV intact. CI builds, lints and tests it in a separate job. The
adapter maps one sub-mix to one `RoarRenderer`:

- the sub-mix becomes a ROAR audio group and each audio element a ROAR
  element (loudspeaker layout, or ambisonics order); binaural-input
  elements become stereo elements rendered world-locked-restricted, so
  they pass through;
- the element mix gain (animated, subblock-accurate) arrives pre-applied
  to the element's planes. Both renderers are linear, so this is
  equivalent and keeps a single implementation of IAMF parameter timing;
- elements that carry a demixing parameter get ROAR's IAMF downmix
  renderer (`DownmixInfo`), and per-frame `dmixp_mode` updates are
  forwarded (ROAR does not expose the default-`w_idx` input, so `None`
  is passed);
- binaural targets map `headphones_rendering_mode` 0/1/2 to ROAR's
  world-locked-restricted (stereo) / world-locked / head-locked modes;
- frames are rendered in blocks of the codec frame size (or its largest
  divisor ≤ 16384); a short final block is zero-padded.

`iamf-roar` also has `iamfdec-roar`, a CLI that decodes a file to WAV
through ROAR with `iamfdec`'s decode options. Its loudness normalization
and limiter run in the decoder before quantization.

Measured agreement on the curated conformance vectors, ROAR vs builtin
(`crates/iamf-roar/tests/roar.rs`, which pins these):

| Case | Agreement |
| --- | --- |
| Channel-based and ambisonics to all 14 loudspeaker sound systems | bit-exact or ≥ 141 dB SNR (max diff ≤ 1.2e-7): ROAR's EAR path uses the same libiamf tables |
| 7.1.4 with demixing info → 5.1.2 / 5.1.4 / 7.1.2 / 3.1.2 (000070, 000082) | 6.6–88 dB: ROAR applies OAR's IAMF downmix renderer where the builtin renders the reconstructed layout through the static matrix — a design difference |
| Ambisonics → sound system H (22.2) | refused with `DecodeError::UnsupportedRenderer` (upstream ROAR bug, below) |
| Binaural, mode 0 | identical where both render stereo; ambisonics inputs differ (ROAR always uses OBR for scene-based elements) |
| Binaural, mode 1 | 37–90 dB: both are independent ports of obr, not bit-identical |
| Binaural, mode 2 | not comparable: ROAR renders head-locked binaural, the builtin renders stereo |

Known upstream ROAR issue (at the pinned commit): in
`src/renderer/ear/ambisonic_to_channel_renderer.rs`
(`element_renderer_render_h2m`) the dual-LFE re-insertion compares the
LFE index against the *source* channel index instead of the destination
index. For sound system H (LFEs at 3 and 9) the channel that belongs in
output 10 (SiL) is written into LFE2 and then zeroed, and output 10
keeps a copy of output 12 (TpFL). Other layouts have at most one LFE
before the last non-LFE channel and are not affected. Until the pinned
revision has a fix, `iamf-roar` refuses scene-based elements to sound
system H, and the test asserts the refusal.

iamf-tools does not publish its OAR integration (`OarLayoutRenderer` is
mentioned in its history but not exported), so this adapter was designed
independently against ROAR's public API.

## Correctness strategy

Every stage is validated against the reference implementations on the
libiamf test-vector suite (`tools/fetch_vectors.sh`): lossless and
same-layout paths must be bit-exact, lossy/cross-implementation paths are
held to LSB-level tolerances, and binaural is compared against obr's own
output. The streaming decoder must match the batch decoder byte-for-byte
under arbitrary input chunking. Where the references disagree with each
other or with the spec, we match iamf-tools (what Chromium ships) and
file an issue (see the `upstream` label).

Generated artifacts (`matrices.rs`, `assets/binaural/*.wav`) come from
`tools/extract_*.py` against pinned upstream revisions — regenerate,
don't hand-edit. Licensing and derivation notes live in `NOTICE`.
