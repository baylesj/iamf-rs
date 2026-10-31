# Contributing

Thanks for your interest! This project is young and changes quickly. For
anything non-trivial, please open an issue to discuss it before you write
code.

## Set up

The root `rust-toolchain.toml` pins Rust 1.97.1. `rustup` installs and
selects it for you inside the repository.

Install libopus and the ALSA headers, then fetch the conformance vectors.
The vectors go in `tests/vectors/`, which git ignores.

```sh
brew install opus            # macOS
# or: sudo apt install libopus-dev libasound2-dev
tools/fetch_vectors.sh
cargo test --workspace --all-features
```

## Checks to run before a pull request

CI runs these checks. Make sure they pass locally:

```sh
cargo fmt --all --check
cargo fmt --manifest-path fuzz/Cargo.toml --check
cargo clippy --locked --workspace --all-targets --all-features -- -D warnings
cargo clippy --manifest-path fuzz/Cargo.toml --all-targets -- -D warnings
cargo test --locked --workspace --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
cargo deny check
```

CI also runs a few more jobs:

- **MSRV:** The workspace crates must still build with Rust 1.85. To check
  locally, run `cargo +1.85 check --locked --workspace --all-features --all-targets`.
- **ROAR:** `crates/iamf-roar` is outside the workspace. If you change it,
  run fmt, clippy and test with `--manifest-path crates/iamf-roar/Cargo.toml`.
- **Fuzz smoke test:** A short run of each fuzz target. See below.
- **Chromium adapter:** Builds the `iamf-capi` staticlib and runs
  `examples/chromium/iamf_rs_decoder_adapter_test.cc` against it.
- **WASM demo:** Rebuilds `iamf-obr-wasm` for `wasm32-unknown-unknown` and
  checks that it matches the committed `docs/obr_binaural.wasm`. If you
  change `iamf-dec`'s binaural code or `iamf-obr-wasm`, rebuild that file
  with the command in [crates/iamf-obr-wasm/README.md](crates/iamf-obr-wasm/README.md).

## Fuzzing

Fuzzing needs `cargo-fuzz` (`cargo install cargo-fuzz`). The `fuzz`
directory has its own `rust-toolchain.toml`, which pins a nightly
toolchain. Run fuzzers from inside that directory:

```sh
cd fuzz
cargo fuzz run parse_obu -- -max_total_time=60
```

The targets are `parse_obu`, `decode_stream`, `decode_batch` and `capi`. A
scheduled nightly CI workflow (`.github/workflows/nightly.yml`) fuzzes for
longer, with debug assertions, and uses the full vector set.

## Ground rules

- **Conformance comes first.** Decoder changes must keep the vector tests
  bit-exact on lossless paths. When reference implementations disagree, we
  match iamf-tools (what Chromium ships) and file an issue. See
  [ARCHITECTURE.md](ARCHITECTURE.md).
- **No `unsafe`** outside `iamf-capi`, `iamf-opus-ffi` and `iamf-obr-wasm`.
  The workspace lints enforce this.
- **Don't edit generated files by hand.** Regenerate `matrices.rs` and
  `assets/binaural/*.wav` with `tools/extract_*.py`, against pinned
  upstream revisions.
- **Every bug fix comes with a regression test** that fails without the
  fix. [docs/review-2026-10.md](docs/review-2026-10.md) lists examples.
- **Test new parser code with bad input.** Add a fuzz corpus seed or unit
  tests that feed it malformed input.

## License

By contributing, you agree that your contributions are licensed under the
BSD 3-Clause Clear License (see [LICENSE](LICENSE) and [NOTICE](NOTICE)).
