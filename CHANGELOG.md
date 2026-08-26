# Changelog

## Unreleased

Modernization and cleanup pass (no decoding behavior changes; the
conformance suite is byte-identical).

- Semver hygiene: `#[non_exhaustive]` on the public enums and settings
  structs; `channels`/`demixer`/`matrices` are crate-private (with
  `MatrixLayout`/`HoaOrder` re-exported); settings are built via
  `Default` + field assignment.
- Hot-path allocation pass: the demixer borrows prerequisite planes and
  moves outputs (was ~14 plane copies per 7.1.4 frame), the streaming
  decoder parses OBUs without per-payload copies, parameter handling
  clones nothing, mix-gain evaluation caches linear endpoints (no
  per-sample `powf`), the limiter runs in place, and the binaural FFT
  reuses scratch.
- Fixed: NaN from zero-duration bezier subblocks; ambisonics order
  underflow on empty input; `channels == 0` panic in the FLAC adapter;
  `iamfplay`'s quantizer rounding divergence; `iamfplay` no longer
  deep-copies the decoded PCM per stream rebuild.
- Dedup: shared quantizers, trim ranges, param-index builder, Opus
  adapter plumbing; dead items removed (`Bs2051M`, unused frame-size
  plumbing, `is_first_substream`).
- Lints: workspace-wide `clippy::pedantic` subset, `unreachable_pub`,
  `missing_debug_implementations`, `rust_2018_idioms`; the FFI crates
  re-declare `undocumented_unsafe_blocks` (every unsafe block now has a
  SAFETY comment); `missing_docs` enforced in `iamf-obu` and
  `iamf-capi`.
- Tooling: CI gains MSRV (1.85), docs-with-warnings-denied, and fuzz
  fmt/clippy jobs; `Cargo.lock` is committed; crates.io metadata
  (keywords, categories, readme, docs.rs config) added; a test pins the
  C header constants to the Rust definitions.
- CLI polish: `iamfdec` rejects unknown flags and parses args linearly;
  `iamfplay` prints `Display` errors.

## 0.2.0 — 2026-08-25

Spec-conformance and Chromium-integration hardening. The C ABI settings
struct gained fields and a function, hence the minor bump.

- **Profile validation** (new `iamf_dec::profile`): a port of iamf-tools'
  `ProfileFilter`. `StreamSettings::requested_profiles` /
  `iamfrs_settings.requested_profiles` are enforced — the sequence
  header's declared profiles must intersect the requested set, and mix
  selection only considers presentations within some requested profile's
  limits (sub-mix count, headphone rendering modes, element types and
  layouts, expanded-layout variants, element/channel budgets, and the
  one-codec-config / uniform frame-size-and-rate rules). The Chromium
  adapter now forwards `Settings::requested_profile_versions`.
- **Loudness normalization and peak limiting in the streaming decoder
  and C ABI** (`loudness_target_db`, `enable_limiter`), not just the
  CLI. Both default off, matching the iamf-tools decoder that Chromium
  ships (libiamf, by contrast, limits at -1 dBFS by default).
- **Subblock-granular parameter timelines** (`params::ParamCursor`):
  demixing and recon-gain parameter blocks whose subblocks span several
  temporal units now apply each subblock to the units it covers, in both
  drivers; previously only the first subblock was used. Durations follow
  libiamf's `(rate + 0.1) / parameter_rate` scaling.
- **Parameter-ID multimap**: a parameter id consumed by several
  definitions (invalid per spec, but previously last-writer-wins) now
  reaches every consumer; the output-mix-gain entry can no longer
  clobber an element parameter.
- **Temporal-unit assembly robustness**: temporal delimiter OBUs are
  checked for unit alignment (a mid-unit delimiter is a
  `CorruptPacket`); frames of one unit must agree on trimming (§3.9) and
  on frame length across elements; parameter application no longer
  assumes the first substream's frame arrives first.
- **`reset_with_new_mix`** (`iamfrs_decoder_reset_with_new_mix`,
  iamf-tools `ResetWithNewMix`): in-place mix/layout switching that
  reuses (and resets) the codec decoders of retained audio elements
  instead of rebuilding the decoder from scratch; the Chromium adapter
  uses it and no longer retains a descriptor copy.
- **Mix selection by id now falls back to automatic selection** when the
  id is absent or unsupported, matching iamf-tools' documented
  `RequestedMix` semantics (previously an error).
- Ported/verified against upstream: libiamf PR #176 (limiter default
  threshold: rename only, value unchanged at -1 dBFS) and PR #168
  (`rendering_config` parsing — not applicable, iamf-rs skips the sized
  extension region); iamf-tools' August 2026 allocation caps are covered
  by existing bounded parsing.
- Tests: ambisonics PROJECTION coverage (unit tests plus conformance
  vectors 000042/000048, new in the curated fetch set); invalid vectors
  000007/000025 asserted rejected (including new Opus version
  validation); sound systems E–H and 9.1.6 render sanity; FLAC/AAC
  adapter tests; pure-Rust vs libopus Opus equivalence (>60 dB SNR);
  synthetic-stream tests for spanning parameter blocks, delimiter
  alignment, trim mismatches, and duplicate parameter ids. Vector-driven
  tests now fail with fetch instructions when vectors are missing
  (`IAMF_VECTORS_OPTIONAL=1` restores skip-with-message).
- Docs: corrected stale notes (binaural status, batch-driver mix gains,
  projection matrix Q-format, milestones).

## 0.1.1 — 2026-08-25

- `tools/iamfplay`: terminal demo player — live playback of `.iamf` and
  IAMF-in-MP4 (built-in minimal ISO-BMFF demuxer), instant sample-aligned
  stereo ⇄ binaural toggle, channel-bed meters, mix cycling, and a
  generated third-order-ambisonics demo scene (`--demo`).
- `tools/fetch_vectors.sh --demo` fetches real produced content from the
  iamf-tools web demo.
- New public API: `iamf_dec::binaural::sh_coeffs`; new C API getter
  `iamfrs_decoder_get_selected_layout`.
- Corrected README performance claims: the pure-Rust `opus-decoder`
  fallback is far below realtime on production content (its transforms
  are naive DFTs, issue #5); the published numbers use `opus-ffi`.
- Rust edition 2024 (MSRV stays 1.85); FFI unsafety is now explicit via
  `unsafe extern` and `#[unsafe(no_mangle)]`.
- Docs: demo section, SECURITY.md, CONTRIBUTING.md, CI badge;
  `extract_matrices.py` takes the libiamf path as an argument.

## 0.1.0 — 2026-08-24

Initial release: a pure-Rust IAMF (Eclipsa Audio) v1.1 decoder.

- OBU parsing, descriptors, parameter blocks (mix gain, demixing,
  recon gain), temporal units, partial-OBU streaming input.
- Codecs: LPCM, Opus (pure-Rust or libopus via `opus-ffi`), FLAC,
  AAC-LC; pluggable `CodecFactory` for integrator-supplied codecs.
- Scalable-channel reconstruction (demixing, recon gain), gain-matrix
  rendering to sound systems A–H plus IAMF extensions, mono, and
  binaural (native port of google/obr's SH-HRIR renderer).
- Mix selection by id or layout, loudness normalization, limiter,
  start/end trimming, Android/WAVE channel reordering.
- `StreamDecoder` streaming API, C ABI (`iamf-capi`), and a reference
  C++ adapter implementing the iamf-tools decoder interface for
  Chromium (`examples/chromium/`).
- Validated against the libiamf conformance vectors (bit-exact on
  lossless paths); fuzzed (`parse_obu`, `decode_stream`); benchmarked
  faster than libiamf and iamf-tools on all measured vectors.
