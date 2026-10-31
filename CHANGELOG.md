# Changelog

## Unreleased

Hardening and fixes from the October 2026 review. Each fix has its own
commit and a regression test; [docs/review-2026-10.md](docs/review-2026-10.md)
has the full list and the issues that may also exist upstream.

Behavior changes integrators may notice:

- **Stricter input checks.** Streams are rejected (or the frame or unit is
  reported and skipped) when:
  - an LPCM, FLAC or AAC sample rate is outside the spec's set;
  - a decoded frame doesn't hold exactly `num_samples_per_frame` samples
    per channel, or has the wrong channel count;
  - a FLAC frame header disagrees with its substream;
  - an explicit audio substream ID is 17 or lower;
  - two different elements of a mix share a substream ID;
  - a scalable layer removes channels;
  - a binaural frame is longer than 65536 samples.
- **Errors no longer stick.** A failing OBU is consumed instead of retried
  on every `decode()`, a failed temporal unit is skipped by every element,
  an incomplete unit is dropped when the next one starts (buffering is
  bounded), and a failed `reset_with_new_mix` can be retried.
- **Limiter.** With `enable_limiter`, the stream is limited as one signal:
  a unit becomes available once the input after it covers the 5 ms
  look-ahead, and the last one after `signal_end_of_decoding`. Streaming
  output now equals limiting the whole stream.
- **Timelines.** Demixing and recon-gain blocks apply from where they
  arrive, keeping the previous value over gaps, so output no longer
  depends on input chunking.
- **Spec fixes.** Element gain offset ranges are relative to the default
  (spec v2.0 §3.7.1.1; iamf-tools checks absolute values). Reserved IAMF
  v1.1 rendering-config extension bytes are ignored. Mono ambisonics
  `channel_mapping` may reuse a decoded channel. Sound system F's UH+180
  speaker is at 45° in the object panner.
- **C ABI.** A panic returns `IAMFRS_ERR_INTERNAL` and poisons the handle
  instead of unwinding across the ABI (with `panic = "unwind"`). The
  Chromium adapter refuses an empty requested profile set.
- **New API.** `ElementGainOffsetConfig::range()`,
  `ParamCursor::advance_to()`, and `PeakLimiter::push()`, `finish()`,
  `released_by_push()` and `released_by_finish()` for streaming.
- **Performance.** Binaural rendering of frames shorter than 64 samples
  costs a bounded amount per sample (one-sample frames were ~3000x
  slower), and the FFT no longer allocates per transform.

Tooling: Rust 1.97.1 is pinned in `rust-toolchain.toml` (MSRV stays 1.85),
the fuzz nightly in `fuzz/rust-toolchain.toml`. The fuzz targets now reach
the v2.0, reset and batch-decoder paths, and a nightly workflow fuzzes with
debug assertions against the full vector set. CI runs the Chromium adapter,
and checks that `docs/obr_binaural.wasm` matches its source. Conformance
vectors come from pinned upstream commits.

Earlier unreleased work, not listed below: IAMF v2.0 rendering (object
panner, two sub mixes, parameter curves, #29), so the note below about
what the decoder "cannot yet render" is out of date; and the
`iamf-obr-wasm` crate with the AudioWorklet demo.

IAMF v2.0 profile limits in `iamf_dec::profile`, ported from iamf-tools
`ProfileFilter`. v2.0 profiles are opt-in, so the defaults decode exactly
as before.

- `ProfileSet` gains `BASE_ADVANCED`, `ADVANCED1` and `ADVANCED2`, and
  `from_profile_number` maps sequence header profiles 3..=5 to them.
  `ProfileSet::all()` now includes them; the new `ProfileSet::all_v1()`
  is the v1.1 set.
- Defaults stay v1.1-only, like iamf-tools `IamfDecoder::Settings`:
  `ProfileSet::default()`, `ProfileSet::from_bits(0)`,
  `StreamSettings::default()` and `iamfrs_settings.requested_profiles = 0`
  all mean `all_v1()`. The decoder cannot yet render everything v2.0
  allows (several sub mixes, object-based elements, the new expanded
  layouts), so a stream that declares only v2.0 profiles still fails with
  `UnsupportedProfile` unless the caller requests them. `iamfdec` also
  requests the v1.1 set.
- The per-mix rules for the v2.0 profiles: up to two sub mixes; up to two
  codec configs per mix (the first sub mix uses one, one of the two must
  be LPCM, and both must share the sample rate and frame size, with the
  AAC-LC rate read from its AudioSpecificConfig); head-locked binaural
  rendering; object-based elements with exactly one position parameter
  matching their object count (no position parameters on other
  elements); no Base-Advanced for mixes that combine object-based and
  other elements; expanded layouts 13..=19; and element/channel budgets
  of 18/18 (Base-Advanced, Advanced1) and 28/28 (Advanced2).
- New `iamf_dec::profile::output_sample_rate(&CodecConfig)`. The AAC-LC
  adapter uses it, which fixes the explicit-frequency (index 0xF)
  AudioSpecificConfig case, read from the wrong bits before.
- C ABI (header-only): `IAMFRS_PROFILE_BASE_ADVANCED`,
  `IAMFRS_PROFILE_ADVANCED1` and `IAMFRS_PROFILE_ADVANCED2` (bits 3..=5).
- Known gap: iamf-tools' stream-level checks are not ported
  (`ValidateMixPresentationsForPrimaryProfile`,
  `ValidateFirstSubMixesUseSameCodecConfig`,
  `ValidateAudioElementsForAdditionalProfile`, and the Base-Advanced rule
  that the first mix presentation must be used).

Modernization and cleanup pass (no decoding behavior changes beyond mix
selection, below; the conformance suite is byte-identical).

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
  SAFETY comment); `missing_docs` enforced across all library crates
  (`iamf-obu`, `iamf-dec`, `iamf-codecs`, `iamf-capi`).
- Tooling: CI gains MSRV (1.85), docs-with-warnings-denied, and fuzz
  fmt/clippy jobs; `Cargo.lock` is committed; crates.io metadata
  (keywords, categories, readme, docs.rs config) added; a test pins the
  C header constants to the Rust definitions.
- CLI polish: `iamfdec` rejects unknown flags and parses args linearly;
  `iamfplay` prints `Display` errors.
- iamf-tools v3.0.0 decoder-API parity (`TrimmingSettings`,
  `OutputLayout::kIAMF_Binaural`). The streaming decoder and C ABI already
  had both (trimming on by default, binaural = 14); this pins and exposes
  them everywhere:
  - C ABI (additive, header-only; no new symbols, `iamfrs_settings`
    layout unchanged and now pinned by a test): `enum
    iamfrs_output_layout` with `IAMFRS_LAYOUT_*` constants numbered like
    `OutputLayout`, ending in `IAMFRS_LAYOUT_BINAURAL = 14` (also exported
    as Rust constants). Docs tie `disable_trim_start` / `disable_trim_end`
    to `TrimmingSettings` and correct `iamfrs_decoder_get_selected_layout`
    (it reports the requested layout, binaural included).
  - `PresentationDecoder::with_trimming(TrimmingSettings)` gives the
    batch decoder the same knob as `StreamSettings::trimming`.
  - `iamfdec`: `--no-trim-start` / `--no-trim-end`, `-s binaural`, and
    `--loudness` finds a mix's binaural `loudness_info`.
  - Chromium adapter: `static_assert`s pin every `OutputLayout` (incl.
    `kIAMF_Binaural`), `OutputSampleType`, and `ChannelOrdering` value to
    the C constants; the iamf-tools header snapshot is refreshed to v3.0.0.

Descriptor-handling fixes ported from iamf-tools (conformant streams
decode unchanged):

- Mix presentation tag restrictions (iamf-tools 25a3dbb3b and the
  existing `content_language` rule): `MixPresentation::tags` drops a
  `content_language` whose value is not a three-character ISO 639-2
  code, and any `content_language` / `content_type` beyond the first
  valid instance (§3.7.5), instead of keeping every tag. Violations are
  ignored, not errors, matching iamf-tools' permissive decoding.
- A non-redundant IA sequence header that follows a redundant copy
  before any temporal unit now replaces it (iamf-tools 23e4565dd);
  previously the first copy won even if it disagreed. Applies to both
  drivers, the C ABI, and `iamfplay` via `Descriptors::collect`.
  Conformance vector test_000079 joins the curated set.

Descriptor-handling fixes ported from iamf-tools (conformant streams
decode unchanged):

- A sub mix with no audio elements (§3.7: num_audio_elements SHALL NOT
  be 0) is never rendered (iamf-tools ac2fff70b). Previously the parser
  rejected the whole descriptor set (even when other mixes were valid),
  and hand-built descriptors "rendered" zero channels at rate 0. Now the
  mix parses, automatic and by-id selection skip it, and an explicit
  `MixSelection::ByIndex` to it, `PresentationDecoder::new` on it, or a
  stream offering nothing else fails with `InvalidDescriptors`
  (`IAMFRS_ERR_CORRUPT_DATA` in the C ABI).
- Mix selection (behavior change for multi-mix streams; every
  conformance vector has one mix and decodes identically):
  `MixSelection::Auto` implements IAMF §7.4.1 creator-preferred
  selection, ported from iamf-tools `FindMixPresentationAndLayout`
  (7dac42316, f6ff4ab6c). Binaural output prefers a mix whose single
  audio element is authored binaural, then one declaring a binaural
  loudness layout, and no longer prefers stereo-declaring mixes; stereo
  output prefers a mix with exactly one stereo layout and one stereo
  element, rendering-mode stereo first; other layouts still take the
  first mix declaring them. New `Descriptors::select_mix_presentation`
  exposes the same selection for the batch `PresentationDecoder` path,
  and `iamfdec` now uses it instead of always decoding the first mix.
- Binaural-input elements (single-layer loudspeaker_layout BINAURAL, the
  2.1.1 case) now decode: like iamf-tools' passthrough renderer, their
  L/R signals pass through unchanged to binaural and stereo output
  (never HRTF-processed, whatever `headphones_rendering_mode` says). As
  iamf-tools has no renderer for them to other loudspeaker layouts,
  mixes containing them are not selectable for those targets (`Auto`
  and unmatched `ById` skip them; `ByIndex` reports
  `DecodeError::UnsupportedProfile`).
- External renderer hook: `iamf_dec::renderer` (`MixRendererFactory`,
  `MixRenderer`, `MixConfig`, `ElementInfo`, `ElementLayout`,
  `ElementFrame`) and `StreamDecoder::new_from_descriptors_with_renderer`
  let another crate replace the streaming decoder's render-and-mix step.
  Decoding, reconstruction, parameter timing, gains, loudness, limiter
  and quantization stay in the decoder; the builtin path is unchanged
  (conformance byte-identical) and `StreamSettings` stays `Copy`. New
  `DecodeError::UnsupportedRenderer` for a refused or failed external
  renderer (C ABI: `IAMFRS_ERR_UNSUPPORTED`; the C ABI itself always uses
  the builtin renderer).
- New unpublished crate `crates/iamf-roar` (excluded from the workspace,
  own `Cargo.lock`, Rust 1.97.1): renders through AOM's ROAR, pinned to
  AOMediaCodec/roar@45ca492, via the hook, plus an `iamfdec-roar` CLI
  with `iamfdec`'s decode options. ROAR matches the builtin bit-exactly
  (or ≥ 141 dB) on every curated vector × loudspeaker layout except the
  documented OAR downmix-renderer cases. Scene-based elements to sound
  system H are refused because of an upstream ROAR 22.2 LFE bug
  (`ARCHITECTURE.md`, "Renderer backends"). Nothing in the workspace
  depends on ROAR, and CI checks that the workspace lockfile never picks
  it up.

IAMF v2.0 syntax parsing in `iamf-obu`, following the v2.0.0 spec and
iamf-tools v3.0.0. The decoder still renders v1.1 profiles only: profile
filtering drops mixes that use object-based or reserved audio elements,
and conformant v1.1 streams decode unchanged.

- New `metadata` module: Metadata OBU (type 24) payloads (ITU-T T.35,
  IAMF tags; reserved `metadata_type`s kept opaque).
- New `position` module: position parameter definitions and parameter
  block payloads (polar, cartesian 8/16-bit, and their dual variants),
  with the step, linear, bezier, inter-linear and inter-bezier animation
  types. Azimuth and elevation are clipped as in iamf-tools.
- Object-based audio elements (`AudioElementConfig::ObjectBased`,
  element type 2). As in iamf-tools, they must have exactly one substream
  and no parameters.
- Reserved audio element types 3..=7 parse to
  `AudioElementConfig::Extension` (previously an `InvalidDescriptor`
  error).
- The rendering-config extension: position parameter definitions,
  `ElementGainOffsetConfig` and `BinauralFilterProfile`. An extension
  that fails to parse is skipped as opaque, but one whose content
  overruns `rendering_config_extension_size` is now an error (it used to
  be skipped unread).
- `MixPresentationOptionalFields`, read when the OBU header sets
  `optional_fields_flag` (`MixPresentation::parse_with_optional_fields`);
  such a mix presentation must carry the tags block.
- Live loudness (`LoudnessInfo::LIVE`, `is_live`) and the sized
  `layout_extension` region (previously skipped).
- `ProfileVersion` (adds Base-Advanced, Advanced-1, Advanced-2),
  `ExpandedLayout` (adds the 10.2.9.3 and 7.1.5.4 families), and
  `HeadphonesRenderingMode` (world-locked and head-locked binaural).
- Breaking and behavior changes:
  - Reserved OBU types 25..=30 no longer return
    `Error::ReservedObuType`; they parse as the new
    `ObuType::Reserved(u8)` with an opaque payload, as v2.0 requires.
  - `AudioElementConfig` is `#[non_exhaustive]` and gains the
    `ObjectBased` and `Extension` variants.
  - New public fields break struct literals:
    `ObuHeader::type_specific_flag`;
    `SubMixElement::binaural_filter_profile`, `position_params`,
    `element_gain_offset`; `MixPresentation::optional_fields`;
    `LoudnessInfo::layout_extension`.
  - The OBU header's trimming fields are read only for audio frames. On
    other OBU types the bit is v2.0's `type_specific_flag`
    (`ObuHeader::optional_fields_flag`, `ObuHeader::is_key_frame`).

Found by running all 333 libiamf test vectors:

- A malformed descriptor OBU other than the sequence header is skipped, as
  in iamf-tools. A bad sequence header still fails.
- The default vector set adds test_000116, 000120, and 000129 (OBUs a
  decoder must skip), and `stream.rs` decodes them end to end.
- `tools/bench_iamf_tools` compares iamf-rs with iamf-tools in one process
  (results in `docs/iamf-tools-comparison.md`). `fetch_vectors.sh` can copy
  vectors and all reference renders from local checkouts
  (`LIBIAMF_TESTS_DIR`, `IAMF_DEMO_DIR`) and fetches loudspeaker renders
  for layouts 0 to 9.

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
