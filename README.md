# iamf-rs

[![CI](https://github.com/baylesj/iamf-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/baylesj/iamf-rs/actions/workflows/ci.yml)
[![License: BSD-3-Clause-Clear](https://img.shields.io/badge/license-BSD--3--Clause--Clear-blue.svg)](LICENSE)
[![MSRV: 1.85](https://img.shields.io/badge/MSRV-1.85-orange.svg)](Cargo.toml)

iamf-rs is a decoder for [IAMF](https://aomediacodec.github.io/iamf/)
(Immersive Audio Model and Formats, also called Eclipsa Audio), written in
Rust. It has the same pipeline as the
[libiamf](https://github.com/AOMediaCodec/libiamf) reference decoder:

```
OBU parser → codec decoders → element reconstructor → renderer → mixer → post-processor
```

The library decodes IAMF v1.1 and v2.0 streams. It also has a
C ABI with the same shape as the iamf-tools decoder API that Chromium uses.

## Try the demo

Put on headphones and run the terminal player:

```sh
cargo run --release -p iamfplay -- --demo
```

The demo plays a generated third-order ambisonics scene: a bee that circles
your head, plus pings from fixed directions. The meters show how the player
folds the 12-channel bed into the 2 channels that you hear.

| Key       | Action                                        |
| --------- | --------------------------------------------- |
| `b`       | Switch between plain stereo and HRTF binaural |
| `m`       | Go to the next mix presentation (if several)  |
| Space     | Pause or resume                               |
| `←` / `→` | Seek back or forward 2 seconds                |
| `q`       | Quit                                          |

The `b` switch is a sample-aligned crossfade. In binaural mode, the scene
moves out of your head.

To play real produced content from the iamf-tools web demo:

```sh
tools/fetch_vectors.sh --demo
cargo run --release -p iamfplay -- tests/demo/Animated_demo_3OA_and_2_0.iamf
```

The player also accepts IAMF-in-MP4 files. The same download includes
`7_1_4_Flac.iamf`, which plays one speaker at a time. It is a channel check,
not a binaural showcase.

By default, `iamfplay` links libopus. Install it before you build:

```sh
brew install opus                            # macOS
sudo apt install libopus-dev libasound2-dev  # Debian or Ubuntu
```

If libopus is not available, build with `--no-default-features` to use the
pure-Rust Opus decoder. That decoder is slow on real content (see
[issue #5](https://github.com/baylesj/iamf-rs/issues/5)).

## Features

- **Profiles:** IAMF v1.1 (simple, base, and base-enhanced) and v2.0
  (base-advanced, advanced-1, and advanced-2) profiles. Profile checks
  follow the iamf-tools `ProfileFilter` rules: requested profile sets,
  sequence header checks, and per-mix limits.
- **Codecs:** Opus, LPCM, FLAC, and AAC-LC. Opus uses a pure-Rust decoder, or
  libopus through the `opus-ffi` feature.
- **Channel-based audio:** Scalable channel audio with demixing, recon gain,
  output gain, and layer selection, plus expanded loudspeaker layouts (`0..=19`,
  covering `5.1.4`, `7.1.4`, `9.1.6`, `10.2.9.3` / 22.2, and `7.1.5.4` channel
  subsets and full layouts).
- **Scene-based audio:** Ambisonics in mono and projection modes, orders 0
  to 4.
- **Object-based audio:** 1- and 2-object audio elements with polar (`Polar`,
  `DualPolar`) and allocentric cartesian (`Cartesian8`, `Cartesian16`,
  `DualCartesian8`, `DualCartesian16`) position parameters, ITU-R BS.2127 §10
  allocentric-to-polar sector mapping, layer-wise 2D VBAP / DBAP loudspeaker
  panning (matching OAR / ROAR `olr`), and 3rd-order ambisonic HRTF binaural
  encoding.
- **Loudspeaker rendering:** All 15 sound systems (including `7.1.5.4`), with
  the libiamf / OAR EAR gain matrices.
- **Binaural rendering:** Headphone output (layout 14) at any supported sample
  rate for channel-based, scene-based, and object-based elements, with
  `Ambient`, `Direct`, and `Reverberant` filter profiles and both world-locked
  (`mode == 1`) and head-locked (`mode == 2`) rendering. This renderer is a
  native port of [google/obr](https://github.com/google/obr).
- **Custom renderers:** The streaming decoder can hand the render step to
  an external renderer through a small hook. The separate `iamf-roar` crate
  uses it to render through [ROAR](https://github.com/AOMediaCodec/roar),
  AOM's Rust Open Audio Renderer. For how ROAR compares to the builtin
  renderer, see [ARCHITECTURE.md](ARCHITECTURE.md).
- **Parameters:** Animated mix gains and object positions (`Step`, `Linear`,
  `Bezier`, `InterLinear`, and `InterBezier`). Demixing, recon gain, mix gain,
  and object position timelines work at subblock granularity, and parameter
  blocks can span temporal units or have mid-stream gaps.
- **Loudness:** Optional loudness normalization and peak limiter. Both are off
  by default, as in the iamf-tools decoder.
- **Batch and streaming decoders:** Both give byte-identical output. The
  streaming decoder accepts partial OBUs and supports reset and seek.
- **Mix selection:** Select a mix presentation by layout, by ID, or by index.
  `reset_with_new_mix` changes the mix or layout in place and keeps the codec
  state.
- **Output options:** The same options as iamf-tools: channel order (IAMF or
  Android/WAVE), automatic or explicit s16le or s32le samples, trim control,
  and a query for the selected mix.
- **Bitstream checks:** Temporal delimiter alignment, the same trim values in
  each temporal unit, and one codec config and frame size per mix.
- **Memory safety:** All crates except the two FFI crates use
  `#![forbid(unsafe_code)]`. CI fuzzes the parser, the streaming decoder, and
  the C ABI.

## Limitations

- The decoder does not resample the output.
- The library does not demux ISO-BMFF (MP4) by design. In Chromium, the
  demuxer supplies the descriptors and temporal units. `iamfplay` has its own
  small MP4 reader.

For open work, see the
[issue tracker](https://github.com/baylesj/iamf-rs/issues). For the full
history, see [CHANGELOG.md](CHANGELOG.md).

## Conformance

The test suite compares output against the libiamf test vectors and against
the libiamf and iamf-tools binaries:

- Same-layout renders and lossless paths are bit-exact.
- Other paths are within a few LSB.
- Binaural output matches obr within 2 LSB.

When the reference implementations disagree, iamf-rs matches iamf-tools,
because Chromium ships iamf-tools. Each disagreement or suspected upstream bug
has an issue (for example,
[#3](https://github.com/baylesj/iamf-rs/issues/3) and
[#4](https://github.com/baylesj/iamf-rs/issues/4)).

## Performance

Measured in August 2026 on Apple M-series hardware with `cargo bench` and
`tools/bench_compare.py`, with Opus decoded by libopus (`opus-ffi`):

- Faster than libiamf v1.1 on every measured path. On Opus content, iamf-rs
  is 1.6× faster.
- 4–7× faster than the iamf-tools decoder.

The pure-Rust Opus decoder is much slower on real content because its
transforms are naive DFTs
([issue #5](https://github.com/baylesj/iamf-rs/issues/5)). If Opus speed
matters, enable `opus-ffi`.

## Crates

| Crate            | Purpose                                                    |
| ---------------- | ---------------------------------------------------------- |
| `iamf-obu`       | Parses OBUs and descriptors from untrusted input.          |
| `iamf-dec`       | Reconstructs, renders, and mixes. Has both decoders.       |
| `iamf-codecs`    | Codec adapters for LPCM, Opus, FLAC, and AAC-LC.           |
| `opus-ffi`       | libopus bindings for the `opus-ffi` feature.               |
| `iamf-capi`      | C ABI over the streaming decoder.                          |
| `iamf-obr-wasm`  | WASM / C ABI over the pure-Rust OBR binaural renderer.     |
| `tools/iamfdec`  | CLI that inspects `.iamf` files and decodes them to WAV.   |
| `tools/iamfplay` | Terminal player with a stereo and binaural toggle.         |
| `fuzz`           | cargo-fuzz targets for the parser, decoder, and C ABI.     |
| `iamf-roar`      | Optional ROAR renderer and its CLI. Not in the workspace.  |

`iamf-obu` has no dependencies. `iamf-dec` loads codecs through the
`SubstreamDecoder` and `CodecFactory` traits, so an embedder can supply its
own. `iamf-codecs` uses [opus-decoder](https://crates.io/crates/opus-decoder)
for pure-Rust Opus and symphonia for FLAC and AAC-LC. `iamf-capi` builds
`libiamf_rs` as a cdylib and a staticlib, with the header in
[`crates/iamf-capi/include/iamf_rs.h`](crates/iamf-capi/include/iamf_rs.h).
For more detail, see [ARCHITECTURE.md](ARCHITECTURE.md).

## Cargo features

By default, the library crates enable all pure-Rust features: the `pcm`,
`opus`, `flac`, and `aac` codecs, plus `binaural`.

The `opus-ffi` feature is opt-in. It decodes Opus through an external
libopus, from the system or from the embedder (for example, Chromium's
`third_party/opus`). When `opus-ffi` is on, `DefaultFactory` uses it instead
of the pure-Rust decoder.

To trim the build, set `default-features = false` and list the features that
you need. This is the build that Chromium needs:

```sh
cargo build -p iamf-capi --no-default-features --features pcm,opus-ffi
```

This build uses only the crates in this repository, so there are no
external Rust dependencies to vendor. Turning off `binaural` also removes the
FFT code and about 1.8 MB of embedded HRIR filters. Binaural output then falls
back to stereo. CI checks these feature sets.

## ROAR renderer

The ROAR renderer lives in its own crate, `crates/iamf-roar`. That crate is
not a workspace member and is not published. Its `RoarRendererFactory` plugs
into `StreamDecoder::new_from_descriptors_with_renderer`. Some notes on this
crate:

- ROAR is not on crates.io. It is a git dependency pinned to a commit.
  Keeping it out of the workspace means that the other crates build offline
  and publish without it.
- ROAR needs Rust 1.97.1. The workspace keeps the 1.85 MSRV.
- ROAR defines unmangled C symbols (`roar_*`), which end up in any C library
  that links it. For this reason, `iamf-capi` does not use ROAR. The C ABI
  and the batch decoder always use the builtin renderer.

The crate includes `iamfdec-roar`, a CLI that decodes a file to WAV through
ROAR. It takes the same decode options as `iamfdec`:

```sh
cargo +1.97.1 run --release --manifest-path crates/iamf-roar/Cargo.toml -- file.iamf -s 3 -o out.wav
```

## Use in Chromium

[`examples/chromium/`](examples/chromium/) has a C++ adapter that implements
the iamf-tools decoder interface on top of the iamf-rs C ABI. With this
adapter, Chromium's `IamfAudioDecoder` can use iamf-rs instead of iamf-tools
without other changes. CI compile-checks the adapter.

## Development

```sh
cargo test                       # unit and conformance tests
cargo bench -p iamfdec           # criterion decode benchmarks
cargo clippy --all-targets       # lints
cargo +nightly fuzz run parse_obu
cargo run -p iamfdec -- file.iamf
```

Conformance vectors go in `tests/vectors/`, which git ignores. To download
them, run one of these commands:

```sh
tools/fetch_vectors.sh               # curated set
tools/fetch_vectors.sh --all         # all vectors (about 333, slow)
tools/fetch_vectors.sh test_000123   # specific vectors
```

When the vectors are present, `cargo test` and `cargo bench` use them.

Before you send a pull request, read [CONTRIBUTING.md](CONTRIBUTING.md). It
lists the checks that CI runs.

## License

iamf-rs uses the BSD 3-Clause Clear License (see [LICENSE](LICENSE)). The
reference implementations that this project derives from use the same
license.

Parts of this code are ported from or derived from libiamf, google/obr, and
iamf-tools. The data tables were extracted mechanically. Several DSP
components are behavioral ports that were written with those sources as
reference. For the full attribution list, see [NOTICE](NOTICE).

The BSD 3-Clause Clear License grants no patent rights. The upstream projects
publish separate patent licenses (AOM Patent License 1.0 and OBR Patent
License 1.0) that cover the derived portions. This section is a good-faith
summary, not legal advice. Get a legal review before you redistribute.

The optional `iamf-roar` crate also links
[ROAR](https://github.com/AOMediaCodec/roar). ROAR uses its own BSD-3-Clause
license plus the AOM Patent License 1.0 (see the `LICENSE` and `PATENTS`
files of ROAR). The workspace crates never include ROAR.
