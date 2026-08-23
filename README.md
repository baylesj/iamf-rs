# iamf-rs

A pure-Rust decoder for [IAMF](https://aomediacodec.github.io/iamf/) (Immersive
Audio Model and Formats / Eclipsa Audio), structured after the pipeline of the
[libiamf](https://github.com/AOMediaCodec/libiamf) reference decoder:

```
OBU parser → codec decoders → element reconstructor → renderer → mixer → post-processor
```

## Status (August 2026)

The decoder is functionally complete for IAMF v1.1 simple/base profile
streams: Opus and LPCM substreams, scalable channel reconstruction
(demixing, recon gains, output gain), ambisonics, rendering to all 14
loudspeaker sound systems, animated mix gains, and an optional loudness /
peak-limiter post stage. Output is checked against the libiamf test-vector
suite: same-layout renders are bit-exact, everything else lands within a
few LSBs of the reference renderer.

There are two ways in: a batch pipeline (`PresentationDecoder`) and a
streaming decoder (`StreamDecoder`) with a C ABI (`iamf-capi`) that mirrors
the iamf-tools API Chromium calls, so it can sit under Chrome's
`IamfAudioDecoder` behind a thin adapter. The streaming path is tested
byte-identical to the batch path, including partial-OBU feeding and
reset/seek.

Known gaps: HRTF binaural (output layout 14 works but renders the same
stereo fallback libiamf uses without its binauralizer), expanded
loudspeaker layouts, AAC-LC/FLAC substreams, multi-sub-mix presentations,
and resampling. Performance: on an Apple M-series laptop, head-to-head
against both C++ implementations on the same vectors, iamf-rs is faster
than libiamf v1.1 on every measured path and 4–7× faster than iamf-tools'
decoder (the implementation Chromium ships), with outputs matching
iamf-tools' binary bit-exactly or within 1 LSB. Opus decode via the
default pure-Rust crate runs ~2× slower than libiamf; the `opus-ffi`
feature (libopus bindings) makes it ~1.6× faster instead. One vector
exposes a genuine disagreement *between* libiamf and iamf-tools; we match
iamf-tools (issue #3).

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
6. **Later** — binaural rendering, AAC-LC/FLAC, expanded layouts, higher
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

MIT or Apache-2.0, at your option. This is an independent implementation
written from the IAMF specification; it does not incorporate code from
libiamf (BSD-3-Clause-Clear + AOM Patent License 1.0), which is used only as
a behavioral reference for conformance testing.
