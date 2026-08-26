# Chromium reference adapter

`IamfRsDecoderAdapter` implements `iamf_tools::api::IamfDecoderInterface`
over the iamf-rs C ABI (`crates/iamf-capi/include/iamf_rs.h`), so
Chromium's `IamfAudioDecoder` can swap iamf-tools for iamf-rs without
modification. It is compile-checked in CI against the header snapshot in
`iamf_tools_api_snapshot/`.

To use in a Chromium checkout:

1. Build `iamf-capi` as a staticlib with the `opus-ffi` feature, pointing
   `OPUS_LIB_DIR` at `third_party/opus` (see `crates/opus-ffi/build.rs`),
   or wire it through a `rust_static_library` GN target.
2. Copy `iamf_rs_decoder_adapter.{h,cc}` next to `IamfAudioDecoder` and
   change the snapshot includes to the in-tree
   `third_party/iamf_tools` headers.
3. Construct via `IamfRsDecoderAdapter::CreateFromDescriptors(...)` where
   the code calls `IamfDecoderFactory::CreateFromDescriptors(...)` —
   the settings struct and semantics are the same.

Notes:

- `IamfStatus::OkStatus` / `ErrorStatus` / `operator<<` are declared but
  not defined in the API headers; in Chromium they come from the
  iamf_tools library. If you build the adapter without iamf-tools, add
  trivial definitions.
- `Settings::requested_profile_versions` is forwarded as
  `iamfrs_settings.requested_profiles` and enforced with iamf-tools
  `ProfileFilter` semantics: the sequence-header profiles must intersect
  the requested set, and only mix presentations within some requested
  profile's limits (sub-mix count, element types and layouts,
  element/channel budgets, codec-config rules) are selectable.
- `ResetWithNewMix` maps to `iamfrs_decoder_reset_with_new_mix`, which
  reconfigures in place and reuses the codec decoders of audio elements
  shared between the old and new mix.
- Loudness normalization and a libiamf-style peak limiter are available
  via `iamfrs_settings` but default off, matching the iamf-tools
  decoder's unprocessed output.
