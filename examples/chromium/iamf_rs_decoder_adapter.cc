/* See iamf_rs_decoder_adapter.h. */
#include "iamf_rs_decoder_adapter.h"

#include <cstddef>
#include <cstdint>
#include <memory>
#include <string>

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
using ::iamf_tools::api::ProfileVersion;
using ::iamf_tools::api::RequestedMix;
using ::iamf_tools::api::SelectedMix;
using ::iamf_tools::api::TrimmingSettings;

IamfStatus StatusOf(int code, const char* what) {
  if (code == IAMFRS_OK) {
    return IamfStatus::OkStatus();
  }
  return IamfStatus::ErrorStatus(std::string(what) + ": iamfrs error " +
                                 std::to_string(code));
}

/* OutputLayout and iamfrs_settings.output_layout share the IAMF
 * sound-system numbering (0 = stereo ... 13 = 9.1.6, 14 = binaural), so
 * values are forwarded by cast. Pin every value, including
 * kIAMF_Binaural (iamf-tools v3.0.0), at compile time. */
constexpr bool LayoutMatches(OutputLayout layout, int value) {
  return static_cast<int>(layout) == value;
}
static_assert(LayoutMatches(OutputLayout::kItu2051_SoundSystemA_0_2_0,
                            IAMFRS_LAYOUT_SOUND_SYSTEM_A_0_2_0),
              "");
static_assert(LayoutMatches(OutputLayout::kItu2051_SoundSystemB_0_5_0,
                            IAMFRS_LAYOUT_SOUND_SYSTEM_B_0_5_0),
              "");
static_assert(LayoutMatches(OutputLayout::kItu2051_SoundSystemC_2_5_0,
                            IAMFRS_LAYOUT_SOUND_SYSTEM_C_2_5_0),
              "");
static_assert(LayoutMatches(OutputLayout::kItu2051_SoundSystemD_4_5_0,
                            IAMFRS_LAYOUT_SOUND_SYSTEM_D_4_5_0),
              "");
static_assert(LayoutMatches(OutputLayout::kItu2051_SoundSystemE_4_5_1,
                            IAMFRS_LAYOUT_SOUND_SYSTEM_E_4_5_1),
              "");
static_assert(LayoutMatches(OutputLayout::kItu2051_SoundSystemF_3_7_0,
                            IAMFRS_LAYOUT_SOUND_SYSTEM_F_3_7_0),
              "");
static_assert(LayoutMatches(OutputLayout::kItu2051_SoundSystemG_4_9_0,
                            IAMFRS_LAYOUT_SOUND_SYSTEM_G_4_9_0),
              "");
static_assert(LayoutMatches(OutputLayout::kItu2051_SoundSystemH_9_10_3,
                            IAMFRS_LAYOUT_SOUND_SYSTEM_H_9_10_3),
              "");
static_assert(LayoutMatches(OutputLayout::kItu2051_SoundSystemI_0_7_0,
                            IAMFRS_LAYOUT_SOUND_SYSTEM_I_0_7_0),
              "");
static_assert(LayoutMatches(OutputLayout::kItu2051_SoundSystemJ_4_7_0,
                            IAMFRS_LAYOUT_SOUND_SYSTEM_J_4_7_0),
              "");
static_assert(LayoutMatches(OutputLayout::kIAMF_SoundSystemExtension_2_7_0,
                            IAMFRS_LAYOUT_EXTENSION_2_7_0),
              "");
static_assert(LayoutMatches(OutputLayout::kIAMF_SoundSystemExtension_2_3_0,
                            IAMFRS_LAYOUT_EXTENSION_2_3_0),
              "");
static_assert(LayoutMatches(OutputLayout::kIAMF_SoundSystemExtension_0_1_0,
                            IAMFRS_LAYOUT_EXTENSION_0_1_0),
              "");
static_assert(LayoutMatches(OutputLayout::kIAMF_SoundSystemExtension_6_9_0,
                            IAMFRS_LAYOUT_EXTENSION_6_9_0),
              "");
static_assert(LayoutMatches(OutputLayout::kIAMF_Binaural,
                            IAMFRS_LAYOUT_BINAURAL),
              "kIAMF_Binaural must map to IAMFRS_LAYOUT_BINAURAL");
static_assert(static_cast<int>(OutputSampleType::kInt16LittleEndian) ==
                      IAMFRS_SAMPLE_INT16_LE &&
                  static_cast<int>(OutputSampleType::kInt32LittleEndian) ==
                      IAMFRS_SAMPLE_INT32_LE,
              "OutputSampleType must match iamfrs_sample_type");
static_assert(static_cast<int>(ChannelOrdering::kIamfOrdering) ==
                      IAMFRS_ORDERING_IAMF &&
                  static_cast<int>(ChannelOrdering::kOrderingForAndroid) ==
                      IAMFRS_ORDERING_ANDROID,
              "ChannelOrdering must match iamfrs_channel_ordering");

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
  /* TrimmingSettings (iamf-tools v3.0.0): the C ABI stores the inverse so
   * that zero-initialized settings keep trimming on, like the defaults. */
  const TrimmingSettings& trimming = settings.trimming_settings;
  c_settings.disable_trim_start = trimming.trim_beginning ? 0 : 1;
  c_settings.disable_trim_end = trimming.trim_end ? 0 : 1;
  /* ProfileVersion values are the profile numbers (simple=0, base=1,
   * base-enhanced=2), which are the iamfrs_profile bit positions. */
  c_settings.requested_profiles = 0;
  for (ProfileVersion profile : settings.requested_profile_versions) {
    c_settings.requested_profiles |= 1u << static_cast<uint32_t>(profile);
  }
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
  return std::unique_ptr<IamfRsDecoderAdapter>(new IamfRsDecoderAdapter(decoder));
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
  const int64_t mix_id =
      requested_mix.mix_presentation_id.has_value()
          ? static_cast<int64_t>(*requested_mix.mix_presentation_id)
          : -1;
  const int32_t layout =
      requested_mix.output_layout.has_value()
          ? static_cast<int32_t>(*requested_mix.output_layout)
          : -1;
  const int code =
      iamfrs_decoder_reset_with_new_mix(decoder_, mix_id, layout);
  if (code != IAMFRS_OK) {
    return StatusOf(code, "ResetWithNewMix");
  }
  return GetOutputMix(selected_mix);
}

IamfStatus IamfRsDecoderAdapter::SignalEndOfDecoding() {
  return StatusOf(iamfrs_decoder_signal_end_of_decoding(decoder_),
                  "SignalEndOfDecoding");
}

}  // namespace iamf_rs
