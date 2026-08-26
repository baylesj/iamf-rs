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
