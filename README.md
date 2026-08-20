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
| `tools/iamfdec` | CLI: inspect (eventually decode to WAV) standalone `.iamf` files. |
| `fuzz` | cargo-fuzz targets for the parser. |

## Milestones

1. **OBU framing** — header/trimming/extension parsing, fuzz target. *(done)*
2. **Descriptors** — codec config, audio element, mix presentation; validated
   against libiamf test vectors (`tools/fetch_vectors.sh`). *(done — parameter
   block data OBUs land with milestone 3)*
3. **Opus path** — wire a pure-Rust Opus decoder; decode simple-profile
   streams substream-by-substream.
4. **Reconstruction & rendering** — demixing/recon gain, loudspeaker layouts
   (stereo, 5.1, 7.1.4), mixing, loudness normalization, peak limiter;
   sample-exactness checked against libiamf conformance output.
5. **Containers & integration** — IAMF-in-ISO-BMFF, C ABI for embedding the
   parser under a C/C++ integration (Chromium-style incremental oxidation).
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
