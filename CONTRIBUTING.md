# Contributing

Thanks for your interest! This project is young and moving quickly, so
it's worth opening an issue to discuss anything non-trivial before
writing code.

## Building and testing

```sh
brew install opus            # or: apt install libopus-dev libasound2-dev
tools/fetch_vectors.sh       # conformance vectors (git-ignored)
cargo test --workspace --all-features
```

Before sending a PR, make sure the same gates CI runs are green:

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo deny check
```

## Ground rules

- **Conformance first.** Decoder changes must keep the vector tests
  bit-exact on lossless paths. Where reference implementations disagree,
  we match iamf-tools (what Chromium ships) and file an issue — see
  ARCHITECTURE.md.
- **No `unsafe`** outside `iamf-capi` and `opus-ffi` (the workspace
  lints enforce this).
- **Generated files** (`matrices.rs`, `assets/binaural/*.wav`) are
  regenerated via `tools/extract_*.py` against pinned upstream
  revisions, never hand-edited.
- New parser surface should come with a fuzz corpus seed or unit tests
  exercising malformed input.

By contributing you agree that your contributions are licensed under the
BSD 3-Clause Clear License (see LICENSE and NOTICE).
