# Changelog

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
