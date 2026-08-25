# Changelog

## 0.1.1 — 2026-08-25

- `tools/iamfplay`: terminal demo player — live playback of `.iamf` and
  IAMF-in-MP4 (built-in minimal ISO-BMFF demuxer), instant sample-aligned
  stereo ⇄ binaural toggle, channel-bed meters, mix cycling, and a
  generated third-order-ambisonics demo scene (`--demo`).
- `tools/fetch_vectors.sh --demo` fetches real produced content from the
  iamf-tools web demo.
- New public API: `iamf_dec::binaural::sh_coeffs`; new C API getter
  `iamfrs_decoder_get_selected_layout`.
- Corrected README performance claims: the pure-Rust `opus-decoder`
  fallback is far below realtime on production content (its transforms
  are naive DFTs, issue #5); the published numbers use `opus-ffi`.
- Rust edition 2024 (MSRV stays 1.85); FFI unsafety is now explicit via
  `unsafe extern` and `#[unsafe(no_mangle)]`.
- Docs: demo section, SECURITY.md, CONTRIBUTING.md, CI badge;
  `extract_matrices.py` takes the libiamf path as an argument.

## 0.1.0 — 2026-08-24

Initial release: a pure-Rust IAMF (Eclipsa Audio) v1.1 decoder.

- OBU parsing, descriptors, parameter blocks (mix gain, demixing,
  recon gain), temporal units, partial-OBU streaming input.
- Codecs: LPCM, Opus (pure-Rust or libopus via `opus-ffi`), FLAC,
  AAC-LC; pluggable `CodecFactory` for integrator-supplied codecs.
- Scalable-channel reconstruction (demixing, recon gain), gain-matrix
  rendering to sound systems A–H plus IAMF extensions, mono, and
  binaural (native port of google/obr's SH-HRIR renderer).
- Mix selection by id or layout, loudness normalization, limiter,
  start/end trimming, Android/WAVE channel reordering.
- `StreamDecoder` streaming API, C ABI (`iamf-capi`), and a reference
  C++ adapter implementing the iamf-tools decoder interface for
  Chromium (`examples/chromium/`).
- Validated against the libiamf conformance vectors (bit-exact on
  lossless paths); fuzzed (`parse_obu`, `decode_stream`); benchmarked
  faster than libiamf and iamf-tools on all measured vectors.
