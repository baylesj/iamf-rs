/* Runtime test of the reference adapter against the iamf-rs staticlib.
 *
 * Build and run (CI does this; see .github/workflows/ci.yml):
 *   cargo build --release -p iamf-capi
 *   c++ -std=c++17 -Iexamples/chromium -Icrates/iamf-capi/include \
 *     examples/chromium/iamf_rs_decoder_adapter.cc \
 *     examples/chromium/iamf_rs_decoder_adapter_test.cc \
 *     target/release/libiamf_rs.a -lpthread -ldl -lm -o adapter_test
 *   ./adapter_test tests/vectors/test_000002.iamf
 *
 * Licensed under the BSD 3-Clause Clear License (see this repository's
 * LICENSE and NOTICE files).
 */
#include <cstdint>
#include <cstdio>
#include <fstream>
#include <iterator>
#include <ostream>
#include <string>
#include <vector>

#include "iamf_rs_decoder_adapter.h"
#include "iamf_tools_api_snapshot/iamf_tools_api_types.h"

/* The snapshot holds only iamf-tools' headers; define the few IamfStatus
 * members the adapter uses. */
namespace iamf_tools {
namespace api {
IamfStatus::IamfStatus(const std::string& message)
    : success(false), error_message(message) {}
IamfStatus IamfStatus::OkStatus() { return IamfStatus(); }
IamfStatus IamfStatus::ErrorStatus(const std::string& message) {
  return IamfStatus(message);
}
std::ostream& operator<<(std::ostream& os, const IamfStatus& status) {
  return os << (status.ok() ? "OK" : status.error_message);
}
}  // namespace api
}  // namespace iamf_tools

namespace {

int failures = 0;

void Check(bool condition, const char* what) {
  if (!condition) {
    std::fprintf(stderr, "FAILED: %s\n", what);
    ++failures;
  }
}

}  // namespace

int main(int argc, char** argv) {
  if (argc != 2) {
    std::fprintf(stderr, "usage: %s test_000002.iamf\n", argv[0]);
    return 2;
  }
  std::ifstream file(argv[1], std::ios::binary);
  const std::vector<uint8_t> data((std::istreambuf_iterator<char>(file)),
                                  std::istreambuf_iterator<char>());
  Check(!data.empty(), "read the vector");

  using iamf_tools::api::IamfDecoderFactory;
  using iamf_tools::api::OutputSampleType;

  /* Regression: an empty profile set used to reach the C ABI as 0, which
   * means "the default v1.1 profiles", while iamf-tools accepts nothing. */
  IamfDecoderFactory::Settings none;
  none.requested_profile_versions.clear();
  Check(iamf_rs::IamfRsDecoderAdapter::CreateFromDescriptors(
            none, data.data(), data.size()) == nullptr,
        "empty requested_profile_versions is refused");

  /* #17: an explicitly requested layout is used as is. */
  {
    IamfDecoderFactory::Settings five_one;
    five_one.requested_mix.output_layout =
        iamf_tools::api::OutputLayout::kItu2051_SoundSystemB_0_5_0;
    auto decoder = iamf_rs::IamfRsDecoderAdapter::CreateFromDescriptors(
        five_one, data.data(), data.size());
    int channels = 0;
    Check(decoder != nullptr &&
              decoder->GetNumberOfOutputChannels(channels).ok() &&
              channels == 6,
          "requested 5.1 gives six channels");
  }

  /* Defaults: no layout requested (the mix's own layout, stereo for
   * test_000002), s32le, trimming on. test_000002 is 8000 frames of 16 kHz
   * stereo LPCM. */
  IamfDecoderFactory::Settings settings;
  auto decoder = iamf_rs::IamfRsDecoderAdapter::CreateFromDescriptors(
      settings, data.data(), data.size());
  Check(decoder != nullptr, "create with default settings");
  if (decoder == nullptr) {
    return 1;
  }
  int channels = 0;
  Check(decoder->GetNumberOfOutputChannels(channels).ok() && channels == 2,
        "two output channels");
  iamf_tools::api::SelectedMix selected;
  Check(decoder->GetOutputMix(selected).ok() &&
            selected.output_layout ==
                iamf_tools::api::OutputLayout::kItu2051_SoundSystemA_0_2_0,
        "an unset layout resolves to the mix's own (stereo)");
  Check(decoder->GetOutputSampleType() == OutputSampleType::kInt32LittleEndian,
        "s32le output");
  Check(decoder->Decode(data.data(), data.size()).ok(), "decode");
  Check(decoder->SignalEndOfDecoding().ok(), "signal end of decoding");
  std::vector<uint8_t> buffer(1 << 16);
  size_t total = 0;
  while (decoder->IsTemporalUnitAvailable()) {
    size_t written = 0;
    Check(decoder->GetOutputTemporalUnit(buffer.data(), buffer.size(), written)
              .ok(),
          "pull a temporal unit");
    total += written;
  }
  Check(total == 8000 * 2 * 4, "8000 frames x 2 channels x 4 bytes");

  if (failures == 0) {
    std::printf("adapter test passed\n");
  }
  return failures == 0 ? 0 : 1;
}
