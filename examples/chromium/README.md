# Chromium reference adapter

`IamfRsDecoderAdapter` lets Chromium use iamf-rs in place of iamf-tools.
It implements `iamf_tools::api::IamfDecoderInterface` on top of the iamf-rs
C ABI (`crates/iamf-capi/include/iamf_rs.h`). Chromium's
`IamfAudioDecoder` can then use iamf-rs with no other changes.

CI compile-checks the adapter against the iamf-tools header snapshot in
`iamf_tools_api_snapshot/`.

## Use it in a Chromium checkout

1. Build `iamf-capi` as a staticlib with the `opus-ffi` feature. Set
   `OPUS_LIB_DIR` to the directory that holds the libopus built from
   `third_party/opus`, and `OPUS_STATIC=1` to link it statically (see
   `crates/opus-ffi/build.rs`). Or wire the crate in through a
   `rust_static_library` GN target.
2. Copy `iamf_rs_decoder_adapter.{h,cc}` next to `IamfAudioDecoder`.
   Change the snapshot includes to the in-tree `third_party/iamf_tools`
   headers.
3. Where the code calls `IamfDecoderFactory::CreateFromDescriptors(...)`,
   call `IamfRsDecoderAdapter::CreateFromDescriptors(...)` instead. The
   settings struct and its meaning are the same.

## API mapping

The adapter matches the iamf-tools v3.0.0 `IamfDecoderInterface`.

| iamf-tools | iamf-rs C ABI |
| --- | --- |
| `Settings::requested_profile_versions` | `iamfrs_settings.requested_profiles` |
| `Settings::trimming_settings` (`TrimmingSettings`) | `iamfrs_settings.disable_trim_start` and `disable_trim_end` |
| `ResetWithNewMix` | `iamfrs_decoder_reset_with_new_mix` |
| `OutputLayout::kIAMF_Binaural` | `IAMFRS_LAYOUT_BINAURAL` (14) |

Details:

- **Profiles.** iamf-rs enforces the requested profiles with the same rules
  as the iamf-tools `ProfileFilter`. The sequence header's profiles must
  overlap the requested set. You can only select mix presentations that fit
  the limits of some requested profile: sub-mix count, element types and
  layouts, element and channel budgets, and codec config rules.
- **Trimming.** The trim flags are inverted (`disable_*`). That way, a
  zero-initialized C settings struct keeps the default, which is to trim.
- **Mix switching.** `iamfrs_decoder_reset_with_new_mix` reconfigures the
  decoder in place. It reuses the codec decoders of audio elements that the
  old and new mix share.
- **Loudness.** `iamfrs_settings` offers loudness normalization and a
  libiamf-style peak limiter. Both are off by default, to match the
  unprocessed output of the iamf-tools decoder.
- **Enums.** `static_assert`s pin every `OutputLayout`, `OutputSampleType`
  and `ChannelOrdering` value to its C constant.

## Building without iamf-tools

The API headers declare `IamfStatus::OkStatus`, `IamfStatus::ErrorStatus`
and `operator<<`, but don't define them. In Chromium, the iamf_tools
library defines them. If you build the adapter without iamf-tools, add
trivial definitions.
