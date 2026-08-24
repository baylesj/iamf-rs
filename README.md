# iamf-rs

A pure-Rust decoder for [IAMF](https://aomediacodec.github.io/iamf/) (Immersive
Audio Model and Formats / Eclipsa Audio), structured after the pipeline of the
[libiamf](https://github.com/AOMediaCodec/libiamf) reference decoder:

```
OBU parser → codec decoders → element reconstructor → renderer → mixer → post-processor
```

## Features

- IAMF v1.1 simple & base profile decoding: OBUs, descriptors, parameter blocks
- Codecs: Opus (pure-Rust, or libopus via `opus-ffi`), LPCM, FLAC, AAC-LC
- Scalable channel audio: demixing, recon gains, output gain, layer selection
- Ambisonics, mono and projection modes, orders 1–4
- Rendering to all 14 loudspeaker sound systems (libiamf v1.1 gain matrices)
- Binaural rendering for headphones — native port of [google/obr](https://github.com/google/obr) (layout 14), any supported sample rate
- Animated mix gains (step/linear/bezier) and per-frame demixing/recon-gain parameters
- Optional loudness normalization and peak limiter post stage
- Batch and streaming decoders (partial-OBU input, reset/seek), byte-identical outputs
- Mix presentation selection: automatic by layout, by id, or by index
- Output options matching iamf-tools: channel ordering (IAMF or Android/WAVE), auto or explicit s16le/s32le, trimming control, selected-mix query
- C ABI (`iamf-capi`) shaped after the iamf-tools API Chromium consumes
- `#![forbid(unsafe_code)]` outside the FFI boundary; parser and full stream decoder fuzzed (CI smoke + local corpus)

Not yet supported: expanded loudspeaker layouts (base-enhanced profile) and
output-rate resampling. Multi-sub-mix presentations are rejected as invalid,
matching the v1.1 spec (`num_sub_mixes` must be 1) and libiamf.

## Status (August 2026)

Conformance: outputs are checked against the libiamf test-vector suite and
against libiamf's and iamf-tools' own binaries — same-layout renders and
lossless paths are bit-exact, everything else lands within a few LSB, and
binaural matches obr within 2 LSB. Where the reference implementations
disagree with each other, we match iamf-tools (what Chromium ships) and
file it (issues #3, #4).

Performance (Apple M-series, `cargo bench` / `tools/bench_compare.py`):
faster than libiamf v1.1 on every measured path, 4–7× faster than
iamf-tools' decoder, ~60× realtime for Opus stereo with the pure-Rust
codec (1.6× faster than libiamf with `opus-ffi`).

## Crates

| Crate | Purpose |
| --- | --- |
| `iamf-obu` | OBU framing and descriptor parsing of untrusted input. `#![forbid(unsafe_code)]`, fuzzed, no dependencies. |
| `iamf-dec` | Pipeline stages: reconstruction, rendering, mixing, loudness/limiting, plus the batch and streaming decoders. Codec layer is pluggable via the `SubstreamDecoder` / `CodecFactory` traits. |
| `iamf-codecs` | Feature-gated `SubstreamDecoder` implementations: LPCM, Opus (pure-Rust [opus-decoder](https://crates.io/crates/opus-decoder) by default, or libopus via the `opus-ffi` feature); FLAC and AAC-LC planned. |
| `iamf-capi` | C ABI (`libiamf_rs` cdylib/staticlib + `include/iamf_rs.h`) over the streaming decoder. |
| `tools/iamfdec` | CLI: inspect and decode/render standalone `.iamf` files to WAV. |
| `fuzz` | cargo-fuzz targets for the parser. |

## Milestones

1. **OBU framing** — header/trimming/extension parsing, fuzz target. *(done)*
2. **Descriptors** — codec config, audio element, mix presentation. *(done)*
3. **Opus path** — substream decode, trimming, parameter blocks. *(done)*
4. **Reconstruction & rendering** — scalable demixing, ambisonics, gain
   matrices from libiamf v1.1.0 (`tools/extract_matrices.py`), animated
   gains, loudness/limiter. *(done)*
5. **Integration surface** — streaming decoder + C ABI shaped after the
   iamf-tools API Chromium consumes. *(done; ISO-BMFF demuxing is out of
   scope — Chromium's demuxer delivers descriptors + temporal units)*
6. **Binaural** — *(done)* native obr-style renderer: speaker/object→HOA
   SH encoding, partitioned FFT convolution with obr's SH-HRIR filters,
   obr peak limiter; validated ≤2 LSB against iamf-tools' `decoder_main
   --output_layout Binaural`. 48 kHz streams only.
7. **Later** — AAC-LC/FLAC, expanded layouts, HRIR resampling, higher
   profiles.

## Development

```sh
cargo test                      # unit + conformance tests
cargo bench -p iamfdec          # criterion decode benchmarks
cargo clippy --all-targets      # lints (unsafe code is forbidden outside iamf-capi)
cargo +nightly fuzz run parse_obu
cargo run -p iamfdec -- file.iamf
```

Conformance vectors go in `tests/vectors/` (git-ignored); populate with
`tools/fetch_vectors.sh` (curated set), `tools/fetch_vectors.sh --all`, or
specific names. `cargo test` and `cargo bench` pick them up automatically
when present.

## License

MIT or Apache-2.0, at your option, for the code, which is an independent
implementation written from the IAMF specification with libiamf used as a
behavioral reference. Two sets of extracted data tables carry their
upstream terms: the rendering gain matrices (from libiamf v1.1.0) and the
SH-HRIR binaural filter assets in `crates/iamf-dec/assets/binaural/`
(from google/obr), both BSD-3-Clause-Clear with their respective AOM/OBR
patent licenses — see `assets/binaural/NOTICE`.
