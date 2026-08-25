/* Reference adapter: implements iamf_tools::api::IamfDecoderInterface on
 * top of the iamf-rs C ABI (iamf_rs.h), so Chromium's IamfAudioDecoder
 * can drive iamf-rs without modification.
 *
 * This is a REFERENCE implementation, compile-checked in this repo's CI
 * against the header snapshot in iamf_tools_api_snapshot/. To use it in
 * Chromium: copy it next to the IamfAudioDecoder, point the includes at
 * the in-tree third_party/iamf_tools headers instead of the snapshot,
 * and link the iamf-rs staticlib (built with the opus-ffi feature
 * against third_party/opus, see crates/opus-ffi/build.rs).
 *
 * Licensed under the BSD 3-Clause Clear License (see this repository's
 * LICENSE and NOTICE files).
 */
#ifndef IAMF_RS_DECODER_ADAPTER_H_
#define IAMF_RS_DECODER_ADAPTER_H_

#include <cstddef>
#include <cstdint>
#include <memory>
#include <vector>

#include "iamf_rs.h"
#include "iamf_tools_api_snapshot/iamf_decoder_factory.h"
#include "iamf_tools_api_snapshot/iamf_decoder_interface.h"
#include "iamf_tools_api_snapshot/iamf_tools_api_types.h"

namespace iamf_rs {

class IamfRsDecoderAdapter : public iamf_tools::api::IamfDecoderInterface {
 public:
  /* Mirrors IamfDecoderFactory::CreateFromDescriptors: `input_buffer`
   * must contain all (and only) the descriptor OBUs. Returns nullptr on
   * failure. Note `Settings::requested_profile_versions` is not
   * consulted: iamf-rs itself validates the sequence-header profiles
   * against IAMF v1.1 (Simple/Base/Base-Enhanced). */
  static std::unique_ptr<IamfRsDecoderAdapter> CreateFromDescriptors(
      const iamf_tools::api::IamfDecoderFactory::Settings& settings,
      const uint8_t* input_buffer, size_t input_buffer_size);

  ~IamfRsDecoderAdapter() override;
  IamfRsDecoderAdapter(const IamfRsDecoderAdapter&) = delete;
  IamfRsDecoderAdapter& operator=(const IamfRsDecoderAdapter&) = delete;

  iamf_tools::api::IamfStatus Decode(const uint8_t* input_buffer,
                                     size_t input_buffer_size) override;
  iamf_tools::api::IamfStatus GetOutputTemporalUnit(
      uint8_t* output_buffer, size_t output_buffer_size,
      size_t& bytes_written) override;
  bool IsTemporalUnitAvailable() const override;
  bool IsDescriptorProcessingComplete() const override;
  iamf_tools::api::IamfStatus GetNumberOfOutputChannels(
      int& output_num_channels) const override;
  iamf_tools::api::IamfStatus GetOutputMix(
      iamf_tools::api::SelectedMix& output_selected_mix) const override;
  iamf_tools::api::OutputSampleType GetOutputSampleType() const override;
  iamf_tools::api::IamfStatus GetSampleRate(
      uint32_t& output_sample_rate) const override;
  iamf_tools::api::IamfStatus GetFrameSize(
      uint32_t& output_frame_size) const override;
  iamf_tools::api::IamfStatus Reset() override;
  iamf_tools::api::IamfStatus ResetWithNewMix(
      const iamf_tools::api::RequestedMix& requested_mix,
      iamf_tools::api::SelectedMix& selected_mix) override;
  iamf_tools::api::IamfStatus SignalEndOfDecoding() override;

 private:
  IamfRsDecoderAdapter(iamfrs_decoder* decoder, iamfrs_settings settings,
                       std::vector<uint8_t> descriptors)
      : decoder_(decoder),
        settings_(settings),
        descriptors_(std::move(descriptors)) {}

  iamfrs_decoder* decoder_;  // Owned.
  /* Kept so ResetWithNewMix can rebuild the decoder with a new target. */
  iamfrs_settings settings_;
  std::vector<uint8_t> descriptors_;
};

}  // namespace iamf_rs

#endif  // IAMF_RS_DECODER_ADAPTER_H_
