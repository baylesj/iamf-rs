# iamf-rs

[![CI](https://github.com/baylesj/iamf-rs/actions/workflows/ci.yml/badge.svg)](https://github.com/baylesj/iamf-rs/actions/workflows/ci.yml)
[![License: BSD-3-Clause-Clear](https://img.shields.io/badge/license-BSD--3--Clause--Clear-blue.svg)](LICENSE)
[![MSRV: 1.85](https://img.shields.io/badge/MSRV-1.85-orange.svg)](Cargo.toml)

iamf-rs is a Rust decoder for [IAMF](https://aomediacodec.github.io/iamf/)
(Immersive Audio Model and Formats), also known as Eclipsa Audio. It decodes
IAMF v1.1 and v2.0 streams.

It follows the same pipeline as the
[libiamf](https://github.com/AOMediaCodec/libiamf) reference decoder:

```
OBU parser → codec decoders → element reconstructor → renderer → mixer → post-processor
```

An OBU (open bitstream unit) is the basic unit of an IAMF stream.

iamf-rs also has a C ABI with the same shape as the iamf-tools decoder API
that Chromium uses. You can use it as a drop-in replacement in Chromium (see
[Use in Chromium](#use-in-chromium)).

## Try the demo

Put on headphones and run the terminal player:

```sh
cargo run --release -p iamfplay -- --demo
```

The demo plays a generated third-order ambisonics scene. A bee circles your
head, and pings sound from fixed directions. The meters show the same scene
rendered to a 12-channel 7.1.4 speaker layout, so you can see where each
sound comes from.

| Key           | Action                                        |
| ------------- | --------------------------------------------- |
| `b`           | Switch between plain stereo and HRTF binaural |
| `m`           | Go to the next mix presentation (if several)  |
| Space         | Pause or resume                               |
| `←` / `→`     | Seek back or forward 2 seconds                |
| `q` or Escape | Quit                                          |

HRTF (head-related transfer function) binaural rendering makes headphone
audio sound like it comes from around you. In binaural mode, the scene moves
out of your head. The `b` switch is a short, sample-aligned crossfade.

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

If you don't have libopus, build with `--no-default-features` to use the
pure-Rust Opus decoder instead. That decoder is slow on real content (see
[issue #5](https://github.com/baylesj/iamf-rs/issues/5)).

## Features

### Formats

- **Profiles:** IAMF v1.1 (simple, base, and base-enhanced) and v2.0
  (base-advanced, advanced-1, and advanced-2). Profile checks follow the
  iamf-tools `ProfileFilter` rules: the requested profile set, sequence
  header checks, and per-mix limits.
- **Codecs:** Opus, LPCM (linear PCM), FLAC, and AAC-LC. Opus uses a
  pure-Rust decoder, or libopus through the `opus-ffi` feature.
- **Channel-based audio:** Scalable channel audio with demixing, recon gain,
  output gain, and layer selection. Expanded loudspeaker layouts `0..=19`
  cover subsets and full versions of `5.1.4`, `7.1.4`, `9.1.6`,
  `10.2.9.3` (22.2), and `7.1.5.4`.
- **Scene-based audio:** Ambisonics in mono and projection modes, orders 0
  to 4.
- **Object-based audio:** Audio elements with 1 or 2 objects. Positions can
  be polar (`Polar`, `DualPolar`) or allocentric cartesian (`Cartesian8`,
  `Cartesian16`, `DualCartesian8`, `DualCartesian16`). Cartesian positions
  map to polar with the ITU-R BS.2127 §10 sector mapping.

### Rendering

- **Loudspeakers:** All 15 sound systems, including `7.1.5.4`. Rendering
  uses the gain matrices from libiamf and the OAR EAR (EBU ADM Renderer)
  tables. Objects use layer-wise 2D VBAP or DBAP
  panning (vector-based or distance-based amplitude panning), matching the
  OAR / ROAR `olr` renderer.
- **Binaural (headphones):** Output layout 14 in the C ABI, at any supported
  sample rate.
  It covers channel-based, scene-based, and object-based elements. You get
  the `Ambient`, `Direct`, and `Reverberant` filter profiles. Elements with
  `headphones_rendering_mode` 1 (world-locked) and 2 (head-locked) both get
  HRTF rendering. Without head tracking the two sound the same, so the
  builtin renderer gives them identical output. This
  renderer is a native port of [google/obr](https://github.com/google/obr),
  the Open Binaural Renderer. Objects are encoded to third-order ambisonics
  before binaural rendering, as in obr.
- **Custom renderers:** The streaming decoder can hand the render step to
  an external renderer through a small hook. The separate `iamf-roar`
  crate uses this hook to render through
  [ROAR](https://github.com/AOMediaCodec/roar), AOM's Rust Open Audio
  Renderer. See [ROAR renderer](#roar-renderer) and
  [ARCHITECTURE.md](ARCHITECTURE.md).
- **Parameters:** Mix gains and object positions can be animated (`Step`,
  `Linear`, `Bezier`, `InterLinear`, and `InterBezier`). Demixing, recon
  gain, mix gain, and object position timelines work at subblock
  granularity. Parameter blocks can span temporal units or leave gaps
  mid-stream.
- **Loudness:** Optional loudness normalization and peak limiter. Both are
  off by default, as in the iamf-tools decoder.

### Decoder API

- **Batch and streaming decoders:** Both give byte-identical output. The
  streaming decoder accepts partial OBUs, and you can reset it to seek.
- **Mix selection:** Pick a mix presentation by output layout, by ID, or by
  index. `reset_with_new_mix` switches the mix or layout in place, without
  parsing the descriptors again. It reuses the codec decoders of audio
  elements that both mixes share.
- **Output options:** The same options as iamf-tools: channel order (IAMF or
  Android/WAVE), automatic or explicit s16le or s32le samples, trim control,
  and a query for the selected mix.
- **Bitstream checks:** Temporal delimiters must sit on temporal unit
  boundaries. All audio frames in a temporal unit must carry the same trim
  values. Each mix must use one codec config and one frame size.

### Safety

- **Memory safety:** The library and tool crates forbid `unsafe` code. The
  exceptions are the three crates that expose or call C: `iamf-capi`,
  `iamf-opus-ffi`, and `iamf-obr-wasm`.
- **Fuzzing:** CI fuzzes the parser, the decoders, and the C ABI. A
  scheduled nightly job fuzzes for longer, with debug assertions, and uses
  the full test vector set.

## Limitations

- The decoder does not resample the output.
- The library does not demux ISO-BMFF (MP4), by design. In Chromium, the
  demuxer supplies the descriptors and temporal units. `iamfplay` has its own
  small MP4 reader.

For open work, see the
[issue tracker](https://github.com/baylesj/iamf-rs/issues). For the full
history, see [CHANGELOG.md](CHANGELOG.md).

## Conformance

The test suite compares iamf-rs output with the libiamf test vectors, and
with the output of the libiamf and iamf-tools binaries:

- Same-layout renders and lossless paths are bit-exact.
- Other paths are within a few LSB (least significant bits).
- Binaural output matches obr within 2 LSB at 16 bits.

When the reference implementations disagree, iamf-rs matches iamf-tools,
because Chromium ships iamf-tools. Each disagreement or suspected upstream
bug has an issue (for example,
[#3](https://github.com/baylesj/iamf-rs/issues/3) and
[#4](https://github.com/baylesj/iamf-rs/issues/4)).

## Performance

These numbers come from August 2026, on Apple M-series hardware. They use
`cargo bench` and `tools/bench_compare.py`, with Opus decoded by libopus
(`opus-ffi`):

- iamf-rs is as fast as or faster than libiamf v1.1 on every measured path.
  On Opus content, it is 1.6× faster.
- On LPCM content, iamf-rs is 4–7× faster than the iamf-tools decoder.

For a side-by-side test against iamf-tools built the way Chromium builds it,
see [docs/iamf-tools-comparison.md](docs/iamf-tools-comparison.md). In that
test, iamf-rs was 4.6–7.8× faster on LPCM and 1.3–2.0× faster on Opus.

The pure-Rust Opus decoder is much slower on real content, because its
transforms are naive DFTs
([issue #5](https://github.com/baylesj/iamf-rs/issues/5)). If Opus speed
matters, enable `opus-ffi`.

## Crates

| Crate           | Path                   | Purpose                                                   |
| --------------- | ---------------------- | --------------------------------------------------------- |
| `iamf-obu`      | `crates/iamf-obu`      | Parses OBUs and descriptors from untrusted input.         |
| `iamf-dec`      | `crates/iamf-dec`      | Reconstructs, renders, and mixes. Has both decoders.      |
| `iamf-codecs`   | `crates/iamf-codecs`   | Codec adapters for LPCM, Opus, FLAC, and AAC-LC.          |
| `iamf-opus-ffi` | `crates/opus-ffi`      | libopus bindings for the `opus-ffi` feature.              |
| `iamf-capi`     | `crates/iamf-capi`     | C ABI over the streaming decoder.                         |
| `iamf-obr-wasm` | `crates/iamf-obr-wasm` | WASM and C ABI over the pure-Rust obr binaural renderer.  |
| `iamfdec`       | `tools/iamfdec`        | CLI that inspects `.iamf` files and decodes them to WAV.  |
| `iamfplay`      | `tools/iamfplay`       | Terminal player with a stereo and binaural toggle.        |
| `iamf-fuzz`     | `fuzz`                 | cargo-fuzz targets for the parser, decoders, and C ABI.   |
| `iamf-roar`     | `crates/iamf-roar`     | Optional ROAR renderer and its CLI. Not in the workspace. |

Some notes on how they fit together:

- `iamf-obu` has no dependencies.
- `iamf-dec` loads codecs through the `SubstreamDecoder` and `CodecFactory`
  traits, so you can supply your own codecs.
- `iamf-codecs` uses [opus-decoder](https://crates.io/crates/opus-decoder)
  for pure-Rust Opus, and symphonia for FLAC and AAC-LC.
- `iamf-capi` builds `libiamf_rs` as a cdylib and a staticlib. The header is
  [`crates/iamf-capi/include/iamf_rs.h`](crates/iamf-capi/include/iamf_rs.h).
- `iamf-obr-wasm` runs the binaural renderer in a browser AudioWorklet. See
  [its README](crates/iamf-obr-wasm/README.md).

For more detail, see [ARCHITECTURE.md](ARCHITECTURE.md).

## Cargo features

By default, the library crates turn on every pure-Rust feature: the `pcm`,
`opus`, `flac`, and `aac` codecs, plus `binaural`.

The `opus-ffi` feature is opt-in. It decodes Opus through an external
libopus, either from the system or from the embedder (for example,
Chromium's `third_party/opus`). When `opus-ffi` is on, `DefaultFactory` uses
it instead of the pure-Rust decoder.

To trim the build, set `default-features = false` and list only the
features you need. This is the build that Chromium needs:

```sh
cargo build -p iamf-capi --no-default-features --features pcm,opus-ffi
```

This build uses only crates from this repository. The one exception is
`pkg-config`, a build dependency of `iamf-opus-ffi`. So it has almost no
external Rust code to vendor.

Turning off `binaural` removes the FFT code and about 3.6 MB of embedded
HRIR (head-related impulse response) filters. Binaural output then falls
back to stereo. CI checks these feature sets.

## ROAR renderer

The ROAR renderer lives in its own crate, `crates/iamf-roar`. Its
`RoarRendererFactory` plugs into
`StreamDecoder::new_from_descriptors_with_renderer`.

The crate is not a workspace member and is not published. Here is why:

- ROAR is not on crates.io. It is a git dependency pinned to a commit.
  Keeping it out of the workspace lets the other crates build offline and
  publish without it.
- ROAR needs Rust 1.97.1. The repository's pinned toolchain is 1.97.1, so
  it builds with plain `cargo`. The workspace crates keep the 1.85 MSRV.
- ROAR defines unmangled C symbols (`roar_*`). These would end up in any C
  library that links it. So `iamf-capi` does not use ROAR. The C ABI and the
  batch decoder always use the builtin renderer.

The crate includes `iamfdec-roar`, a CLI that decodes a file to WAV through
ROAR. It takes the same decode options as `iamfdec`. Run it from the
repository root:

```sh
cargo run --release --manifest-path crates/iamf-roar/Cargo.toml -- file.iamf -s 3 -o out.wav
```

## Use in Chromium

[`examples/chromium/`](examples/chromium/) has a C++ adapter. It implements
the iamf-tools decoder interface on top of the iamf-rs C ABI. With it,
Chromium's `IamfAudioDecoder` can use iamf-rs instead of iamf-tools, with no
other changes. CI compile-checks the adapter.

## Development

### Toolchain

The root `rust-toolchain.toml` pins Rust 1.97.1. Inside the repository,
`rustup` picks it up automatically, so plain `cargo` works. The minimum
supported Rust version (MSRV) for the workspace crates is still 1.85, and
CI checks it.

### Common commands

```sh
cargo test                       # unit and conformance tests
cargo bench -p iamfdec           # criterion decode benchmarks
cargo clippy --all-targets       # lints
cargo run -p iamfdec -- file.iamf
```

### Test vectors

Conformance vectors go in `tests/vectors/`, which git ignores. Download them
with one of these commands:

```sh
tools/fetch_vectors.sh               # curated set
tools/fetch_vectors.sh --all         # all vectors (about 333, slow)
tools/fetch_vectors.sh test_000123   # specific vectors
```

When the vectors are present, `cargo test` and `cargo bench` use them.

### Fuzzing

Fuzzing needs `cargo-fuzz` (`cargo install cargo-fuzz`). The `fuzz`
directory has its own `rust-toolchain.toml` that pins a nightly toolchain.
Run fuzzers from inside that directory:

```sh
cd fuzz
cargo fuzz run parse_obu
```

The targets are `parse_obu` (parser), `decode_stream` (streaming decoder),
`decode_batch` (batch decoder), and `capi` (C ABI).

### Pull requests

Before you send a pull request, read [CONTRIBUTING.md](CONTRIBUTING.md). It
lists the checks that CI runs.

## License

iamf-rs uses the BSD 3-Clause Clear License (see [LICENSE](LICENSE)). The
reference implementations that this project derives from use the same
license.

Parts of this code are ported or derived from libiamf, google/obr, and
iamf-tools. The data tables were extracted mechanically. Several DSP
components are behavioral ports, written with those sources as a reference.
For the full attribution list, see [NOTICE](NOTICE).

The BSD 3-Clause Clear License grants no patent rights. The upstream
projects publish separate patent licenses that cover the derived portions:
the AOM Patent License 1.0 and the OBR Patent License 1.0. This section is a
good-faith summary, not legal advice. Get a legal review before you
redistribute.

The optional `iamf-roar` crate also links
[ROAR](https://github.com/AOMediaCodec/roar). ROAR uses its own BSD-3-Clause
license plus the AOM Patent License 1.0 (see ROAR's `LICENSE` and `PATENTS`
files). The workspace crates never include ROAR.
