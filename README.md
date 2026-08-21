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

Known gaps: binaural output, expanded loudspeaker layouts, AAC-LC/FLAC
substreams, multi-sub-mix presentations, and resampling. Performance is
comfortable with no SIMD work yet — `cargo bench` shows roughly 60×
realtime for Opus stereo (codec-dominated) and 300–800× for LPCM surround
pipelines on an Apple M-series laptop. Head-to-head against libiamf v1.1's
C decoder (`tools/bench_compare.py`), the DSP-heavy paths are at parity or
faster (1.5× on 7.1.4 passthrough) while Opus streams run ~2× slower — the
pure-Rust Opus decoder vs libopus's hand-tuned assembly — and outputs
match libiamf bit-exactly or within 1 LSB, limiter included (except one
vector where libiamf and iamf-tools disagree with each other; we match
iamf-tools — see issue #3).

## Crates

| Crate | Purpose |
| --- | --- |
| `iamf-obu` | OBU framing and descriptor parsing of untrusted input. `#![forbid(unsafe_code)]`, fuzzed, no dependencies. |
| `iamf-dec` | Pipeline stages: reconstruction, rendering, mixing, loudness/limiting, plus the batch and streaming decoders. Codec layer is pluggable via the `SubstreamDecoder` / `CodecFactory` traits. |
| `iamf-codecs` | Feature-gated `SubstreamDecoder` implementations: LPCM, Opus (pure-Rust [opus-decoder](https://crates.io/crates/opus-decoder)); FLAC and AAC-LC planned. |
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
