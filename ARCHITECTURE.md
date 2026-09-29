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
  `render` applies gain matrices extracted from libiamf v1.1.0
  (`matrices.rs`, generated — do not edit); `binaural/` is a native port
  of google/obr (SH encoder → HOA bed → partitioned FFT convolution with
  embedded SH-HRIR filters → limiter); `params` evaluates animated gains
  and carries the subblock-granular parameter timelines (`ParamCursor`);
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

## Renderer backends

`iamf-dec::renderer::RendererBackend` selects how reconstructed audio
elements become output channels (`StreamSettings::renderer`):

- **`Builtin`** (default) — the libiamf-v1.1-matrix renderer and the obr
  binaural port described above. This is the conformance path; it is
  bit-exact against the reference decoders and is what the C ABI and
  the batch `PresentationDecoder` always use.
- **`Roar`** (cargo feature `iamf-dec/roar`, off by default) — AOM's
  [ROAR](https://github.com/AOMediaCodec/roar), the Rust port of the Open
  Audio Renderer that IAMF v2 references, pinned to a git commit
  (`crates/iamf-dec/Cargo.toml`). It needs Rust 1.97.1, above the
  workspace MSRV, so the feature is excluded from the MSRV CI job.

The split is at the renderer, not the decoder: parsing, codec decode,
reconstruction (demixing, recon gain, ambisonics), parameter timelines,
the output-mix gain, loudness normalization and the limiter are shared.
`src/roar.rs` adapts one mix presentation to one `RoarRenderer`:

- each sub-mix becomes a ROAR audio group (ROAR allows two; more is
  `DecodeError::UnsupportedRenderer`) and each audio element a ROAR
  element (loudspeaker layout, or ambisonics order);
- the element mix gain (animated, subblock-accurate) is pre-applied to
  the element's planes before handing them to ROAR — both renderers are
  linear, so this is equivalent and keeps a single implementation of
  IAMF parameter timing;
- elements that carry a demixing parameter get ROAR's IAMF downmix
  renderer (`DownmixInfo`), and per-frame `dmixp_mode` updates are
  forwarded (ROAR does not expose the default-`w_idx` input, so `None`
  is passed);
- binaural targets map `headphones_rendering_mode` 0/1/2 to ROAR's
  world-locked-restricted (stereo) / world-locked / head-locked modes;
- frames are rendered in blocks of the codec frame length (or its
  largest divisor ≤ 16384); a short final block is zero-padded.

Measured agreement on the curated conformance vectors, ROAR vs builtin
(`tools/iamfdec/tests/roar.rs`, which pins these):

| Case | Agreement |
| --- | --- |
| Channel-based and ambisonics to all 14 loudspeaker sound systems | bit-exact or ≥ 141 dB SNR (max diff ≤ 1.2e-7): ROAR's EAR path uses the same libiamf tables |
| 7.1.4 with demixing info → 5.1.2 / 5.1.4 / 7.1.2 / 3.1.2 (000070, 000082) | 6.6–88 dB: ROAR applies OAR's IAMF downmix renderer where the builtin renders the reconstructed layout through the static matrix — a design difference |
| Ambisonics → sound system H (22.2) | all channels but 10 match; channel 10 differs (upstream ROAR bug, below) |
| Binaural, mode 0 | identical where both render stereo; ambisonics inputs differ (ROAR always uses OBR for scene-based elements) |
| Binaural, mode 1 | 37–90 dB: both are independent ports of obr, not bit-identical |

Known upstream ROAR issue (at the pinned commit): in
`ambisonic_to_channel_renderer.rs` the dual-LFE re-insertion compares
the LFE index against the *source* index, so for sound system H (LFEs at
3 and 9) output channel 10 (SiL) carries the source's channel 10 and SiL
is lost. The test asserts this exact shape so a fixed ROAR revision
will be noticed.

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
