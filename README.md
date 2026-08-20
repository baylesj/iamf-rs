# iamf-rs

A pure-Rust decoder for [IAMF](https://aomediacodec.github.io/iamf/) (Immersive
Audio Model and Formats / Eclipsa Audio), structured after the pipeline of the
[libiamf](https://github.com/AOMediaCodec/libiamf) reference decoder:

```
OBU parser → codec decoders → element reconstructor → renderer → mixer → post-processor
```

## Crates

| Crate | Purpose |
| --- | --- |
| `iamf-obu` | OBU framing and descriptor parsing of untrusted input. `#![forbid(unsafe_code)]`, fuzzed, no dependencies. |
| `iamf-dec` | Pipeline stages: reconstruction, rendering, mixing, loudness/limiting. Codec layer is pluggable via the `SubstreamDecoder` / `CodecFactory` traits. |
| `iamf-codecs` | Feature-gated `SubstreamDecoder` implementations (LPCM today; Opus, FLAC, AAC-LC planned via existing pure-Rust codec crates). |
| `iamf-capi` | C ABI (`libiamf_rs` cdylib/staticlib + `include/iamf_rs.h`) over the streaming decoder, shaped after the iamf-tools iterative decoder API that Chromium's `IamfAudioDecoder` consumes. |
| `tools/iamfdec` | CLI: inspect and decode/render standalone `.iamf` files to WAV. |
| `fuzz` | cargo-fuzz targets for the parser. |

## Milestones

1. **OBU framing** — header/trimming/extension parsing, fuzz target. *(done)*
2. **Descriptors** — codec config, audio element, mix presentation; validated
   against libiamf test vectors (`tools/fetch_vectors.sh`). *(done — parameter
   block data OBUs land with milestone 3)*
3. **Opus path** — decode simple-profile streams substream-by-substream:
   Opus via the pure-Rust [opus-decoder](https://crates.io/crates/opus-decoder)
   crate (RFC 8251 conformant, no unsafe), LPCM natively; per-frame trimming;
   parameter block parsing (mix gain, demixing, recon gain);
   `iamfdec -o out.wav`. *(done — LPCM output verified bit-exact against
   libiamf's rendered reference)*
4. **Reconstruction & rendering** — *(done)* substream→channel
   reconstruction including scalable multi-layer demixing (demix chain,
   demixing modes, w-index state, recon-gain smoothing, layer output_gain,
   playback-layout layer selection), ambisonics mono and projection,
   rendering via gain matrices extracted from libiamf v1.1.0
   (`tools/extract_matrices.py`), per-frame demixing/recon-gain parameter
   blocks, animated element/output mix gains (step/linear/bezier),
   multi-element mixing, and optional loudness normalization + look-ahead
   peak limiter (`--loudness dB`, `--limiter`). Validated against reference
   rendered WAVs: same-layout paths bit-exact, cross-layout and demixed
   paths within ±1..3 LSB (references come from iamf-tools, whose float
   arithmetic differs at the last bit; its bezier evaluation differs more,
   max seen 23 LSB). Not yet supported: expanded/binaural input layouts,
   multiple sub mixes.
5. **Integration surface** — *(done)* streaming decoder
   (`iamf_dec::stream::StreamDecoder`) mirroring the iamf-tools iterative
   API Chromium consumes: create from a descriptor blob, push arbitrary
   byte chunks (partial OBUs buffered), pull temporal units as interleaved
   s16le/s32le PCM, reset, end-of-stream; byte-identical to the batch
   pipeline under all chunkings (equivalence-tested). `iamf-capi` exposes
   it over a C ABI with a hand-written header, verified from a real C
   program against the cdylib. IAMF-in-ISO-BMFF demuxing remains out of
   scope (Chromium's demuxer delivers descriptor blob + temporal units).
6. **Later** — binaural rendering, AAC-LC/FLAC paths, higher profiles.

## Development

```sh
cargo test                      # unit tests
cargo clippy --all-targets      # lints (unsafe code is forbidden workspace-wide)
cargo +nightly fuzz run parse_obu
cargo run -p iamfdec -- file.iamf
```

Conformance vectors go in `tests/vectors/` (git-ignored); populate with
`tools/fetch_vectors.sh` (curated set), `tools/fetch_vectors.sh --all`, or
specific names. `cargo test` picks them up automatically when present.

## License

MIT or Apache-2.0, at your option. This is an independent implementation
written from the IAMF specification; it does not incorporate code from
libiamf (BSD-3-Clause-Clear + AOM Patent License 1.0), which is used only as
a behavioral reference for conformance testing.
