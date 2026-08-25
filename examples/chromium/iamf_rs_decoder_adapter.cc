/* See iamf_rs_decoder_adapter.h. */
#include "iamf_rs_decoder_adapter.h"

#include <cstddef>
#include <cstdint>
#include <memory>
#include <string>
#include <utility>
#include <vector>

#include "iamf_rs.h"
#include "iamf_tools_api_snapshot/iamf_decoder_factory.h"
#include "iamf_tools_api_snapshot/iamf_decoder_interface.h"
#include "iamf_tools_api_snapshot/iamf_tools_api_types.h"

namespace iamf_rs {
namespace {

using ::iamf_tools::api::ChannelOrdering;
using ::iamf_tools::api::IamfStatus;
using ::iamf_tools::api::OutputLayout;
using ::iamf_tools::api::OutputSampleType;
using ::iamf_tools::api::RequestedMix;
using ::iamf_tools::api::SelectedMix;

IamfStatus StatusOf(int code, const char* what) {
  if (code == IAMFRS_OK) {
    return IamfStatus::OkStatus();
  }
  return IamfStatus::ErrorStatus(std::string(what) + ": iamfrs error " +
                                 std::to_string(code));
}

/* OutputLayout and iamfrs_settings.output_layout share the IAMF
 * sound-system numbering (0 = stereo ... 13 = 9.1.6, 14 = binaural). */
iamfrs_settings SettingsToC(
    const iamf_tools::api::IamfDecoderFactory::Settings& settings) {
  iamfrs_settings c_settings = {};
  c_settings.output_layout = static_cast<int32_t>(
      settings.requested_mix.output_layout.value_or(
          OutputLayout::kItu2051_SoundSystemA_0_2_0));
  /* OutputSampleType values match iamfrs_sample_type (1 = s16, 2 = s32). */
  c_settings.sample_type =
      static_cast<int32_t>(settings.requested_output_sample_type);
  c_settings.mix_presentation_id =
      settings.requested_mix.mix_presentation_id.has_value()
          ? static_cast<int64_t>(*settings.requested_mix.mix_presentation_id)
          : -1;
  c_settings.channel_ordering =
      static_cast<int32_t>(settings.channel_ordering);
  c_settings.disable_trim_start =
      settings.trimming_settings.trim_beginning ? 0 : 1;
  c_settings.disable_trim_end = settings.trimming_settings.trim_end ? 0 : 1;
  return c_settings;
}

}  // namespace

// static
std::unique_ptr<IamfRsDecoderAdapter> IamfRsDecoderAdapter::CreateFromDescriptors(
    const iamf_tools::api::IamfDecoderFactory::Settings& settings,
    const uint8_t* input_buffer, size_t input_buffer_size) {
  if (input_buffer == nullptr || input_buffer_size == 0) {
    return nullptr;
  }
  const iamfrs_settings c_settings = SettingsToC(settings);
  iamfrs_decoder* decoder = nullptr;
  if (iamfrs_decoder_create_from_descriptors(input_buffer, input_buffer_size,
                                             &c_settings,
                                             &decoder) != IAMFRS_OK) {
    return nullptr;
  }
  return std::unique_ptr<IamfRsDecoderAdapter>(new IamfRsDecoderAdapter(
      decoder, c_settings,
      std::vector<uint8_t>(input_buffer, input_buffer + input_buffer_size)));
}

IamfRsDecoderAdapter::~IamfRsDecoderAdapter() {
  iamfrs_decoder_destroy(decoder_);
}

IamfStatus IamfRsDecoderAdapter::Decode(const uint8_t* input_buffer,
                                        size_t input_buffer_size) {
  return StatusOf(
      iamfrs_decoder_decode(decoder_, input_buffer, input_buffer_size),
      "Decode");
}

IamfStatus IamfRsDecoderAdapter::GetOutputTemporalUnit(
    uint8_t* output_buffer, size_t output_buffer_size, size_t& bytes_written) {
  const int code = iamfrs_decoder_get_output_temporal_unit(
      decoder_, output_buffer, output_buffer_size, &bytes_written);
  /* iamf-tools reports "no unit ready" as success with 0 bytes written. */
  if (code == IAMFRS_ERR_NO_TEMPORAL_UNIT) {
    bytes_written = 0;
    return IamfStatus::OkStatus();
  }
  return StatusOf(code, "GetOutputTemporalUnit");
}

bool IamfRsDecoderAdapter::IsTemporalUnitAvailable() const {
  return iamfrs_decoder_is_temporal_unit_available(decoder_) == 1;
}

bool IamfRsDecoderAdapter::IsDescriptorProcessingComplete() const {
  /* Descriptors are always fully processed at creation time. */
  return true;
}

IamfStatus IamfRsDecoderAdapter::GetNumberOfOutputChannels(
    int& output_num_channels) const {
  uint32_t channels = 0;
  const int code = iamfrs_decoder_get_num_output_channels(decoder_, &channels);
  if (code == IAMFRS_OK) {
    output_num_channels = static_cast<int>(channels);
  }
  return StatusOf(code, "GetNumberOfOutputChannels");
}

IamfStatus IamfRsDecoderAdapter::GetOutputMix(
    SelectedMix& output_selected_mix) const {
  uint32_t mix_id = 0;
  uint32_t layout = 0;
  int code = iamfrs_decoder_get_selected_mix_presentation_id(decoder_, &mix_id);
  if (code == IAMFRS_OK) {
    code = iamfrs_decoder_get_selected_layout(decoder_, &layout);
  }
  if (code == IAMFRS_OK) {
    output_selected_mix.mix_presentation_id = mix_id;
    output_selected_mix.output_layout = static_cast<OutputLayout>(layout);
  }
  return StatusOf(code, "GetOutputMix");
}

OutputSampleType IamfRsDecoderAdapter::GetOutputSampleType() const {
  uint32_t sample_type = 0;
  if (iamfrs_decoder_get_sample_type(decoder_, &sample_type) != IAMFRS_OK ||
      sample_type != static_cast<uint32_t>(OutputSampleType::kInt16LittleEndian)) {
    return OutputSampleType::kInt32LittleEndian;
  }
  return OutputSampleType::kInt16LittleEndian;
}

IamfStatus IamfRsDecoderAdapter::GetSampleRate(
    uint32_t& output_sample_rate) const {
  return StatusOf(
      iamfrs_decoder_get_sample_rate(decoder_, &output_sample_rate),
      "GetSampleRate");
}

IamfStatus IamfRsDecoderAdapter::GetFrameSize(
    uint32_t& output_frame_size) const {
  return StatusOf(iamfrs_decoder_get_frame_size(decoder_, &output_frame_size),
                  "GetFrameSize");
}

IamfStatus IamfRsDecoderAdapter::Reset() {
  return StatusOf(iamfrs_decoder_reset(decoder_), "Reset");
}

IamfStatus IamfRsDecoderAdapter::ResetWithNewMix(
    const RequestedMix& requested_mix, SelectedMix& selected_mix) {
  /* The iamf-rs settings are fixed at creation, so a mix change rebuilds
   * the decoder from the retained descriptor blob. */
  iamfrs_settings c_settings = settings_;
  if (requested_mix.output_layout.has_value()) {
    c_settings.output_layout =
        static_cast<int32_t>(*requested_mix.output_layout);
  }
  c_settings.mix_presentation_id =
      requested_mix.mix_presentation_id.has_value()
          ? static_cast<int64_t>(*requested_mix.mix_presentation_id)
          : -1;
  iamfrs_decoder* decoder = nullptr;
  const int code = iamfrs_decoder_create_from_descriptors(
      descriptors_.data(), descriptors_.size(), &c_settings, &decoder);
  if (code != IAMFRS_OK) {
    return StatusOf(code, "ResetWithNewMix");
  }
  iamfrs_decoder_destroy(decoder_);
  decoder_ = decoder;
  settings_ = c_settings;
  return GetOutputMix(selected_mix);
}

IamfStatus IamfRsDecoderAdapter::SignalEndOfDecoding() {
  return StatusOf(iamfrs_decoder_signal_end_of_decoding(decoder_),
                  "SignalEndOfDecoding");
}

}  // namespace iamf_rs
