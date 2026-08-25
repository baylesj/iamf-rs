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

## Demo

Put on headphones and run the terminal player:

```sh
cargo run --release -p iamfplay -- --demo
```

`--demo` plays a generated third-order ambisonics scene — a bee orbiting
your head with pings from fixed directions. Press `b` to flip between
plain stereo and HRTF binaural mid-stream — the switch is a
sample-aligned crossfade, so listen for the scene moving out of your
head. Space pauses, `←`/`→` seek, `q` quits. The meters show the
12-channel bed being folded into the 2 channels you hear.

For real produced content (from the iamf-tools web demo):

```sh
tools/fetch_vectors.sh --demo
cargo run --release -p iamfplay -- tests/demo/Animated_demo_3OA_and_2_0.iamf
```

The player also accepts IAMF-in-MP4 files. (`7_1_4_Flac.iamf` in the
same set is a one-speaker-at-a-time channel check — educational, but not
much of a binaural showcase.)

Building `iamfplay` needs libopus (`brew install opus` /
`apt install libopus-dev libasound2-dev`); pass `--no-default-features`
to use the pure-Rust Opus decoder instead (slow, see issue #5).

## Status (August 2026)

Conformance: outputs are checked against the libiamf test-vector suite and
against libiamf's and iamf-tools' own binaries — same-layout renders and
lossless paths are bit-exact, everything else lands within a few LSB, and
binaural matches obr within 2 LSB. Where the reference implementations
disagree with each other, we match iamf-tools (what Chromium ships) and
file it (issues #3, #4).

Performance (Apple M-series, `cargo bench` / `tools/bench_compare.py`):
faster than libiamf v1.1 on every measured path and 4–7× faster than
iamf-tools' decoder, with the `opus-ffi` (libopus) codec path (1.6×
faster than libiamf on Opus). The pure-Rust `opus-decoder` fallback is
currently much slower on real content — its transforms are naive DFTs
(issue #5) — so builds that care about Opus throughput should enable
`opus-ffi`.

## Crates

| Crate | Purpose |
| --- | --- |
| `iamf-obu` | OBU framing and descriptor parsing of untrusted input. `#![forbid(unsafe_code)]`, fuzzed, no dependencies. |
| `iamf-dec` | Pipeline stages: reconstruction, rendering, mixing, loudness/limiting, plus the batch and streaming decoders. Codec layer is pluggable via the `SubstreamDecoder` / `CodecFactory` traits. |
| `iamf-codecs` | Feature-gated `SubstreamDecoder` implementations: LPCM, Opus (pure-Rust [opus-decoder](https://crates.io/crates/opus-decoder) by default, or libopus via the `opus-ffi` feature); FLAC and AAC-LC planned. |
| `iamf-capi` | C ABI (`libiamf_rs` cdylib/staticlib + `include/iamf_rs.h`) over the streaming decoder. |
| `tools/iamfdec` | CLI: inspect and decode/render standalone `.iamf` files to WAV. |
| `tools/iamfplay` | Terminal player: live playback (`.iamf` or IAMF-in-MP4) with instant stereo ⇄ binaural toggle, channel meters, and a generated 3OA demo scene (`--demo`). Headphones recommended. |
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

BSD 3-Clause Clear License (see `LICENSE`), matching the reference
implementations this project derives from. Parts of this codebase are
ported from or derived from libiamf, google/obr, and iamf-tools — data
tables were extracted mechanically and several DSP components are
behavioral ports written with those sources as reference. See `NOTICE`
for the full attribution list. The BSD 3-Clause Clear License grants no
patent rights; the upstream projects publish separate patent licenses
(AOM Patent License 1.0, OBR Patent License 1.0) covering the derived
portions. This section is a good-faith summary, not legal advice — have
counsel review before redistribution.
