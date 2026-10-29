// In-process benchmark of iamf-tools against iamf-rs, both driven through the
// iamf_tools::api::IamfDecoderInterface that Chromium uses, with the decoder
// settings Chromium passes (Int32 output, kOrderingForAndroid).
// Reports output parity, throughput, per-chunk latency, and heap allocations.
// Build and run with tools/bench_iamf_tools.py.

#include <dlfcn.h>
#include <malloc.h>

#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstddef>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <iomanip>
#include <iostream>
#include <memory>
#include <numeric>
#include <optional>
#include <string>
#include <vector>

#include "absl/base/log_severity.h"
#include "absl/log/globals.h"
#include "absl/log/initialize.h"
#include "examples/chromium/iamf_rs_decoder_adapter.h"
#include "examples/chromium/iamf_tools_api_snapshot/iamf_decoder_factory.h"
#include "examples/chromium/iamf_tools_api_snapshot/iamf_decoder_interface.h"
#include "examples/chromium/iamf_tools_api_snapshot/iamf_tools_api_types.h"

// ============================================================================
// Heap Allocation Interposition (glibc)
// ============================================================================

extern "C" {
void* __libc_malloc(size_t);
void* __libc_calloc(size_t, size_t);
void* __libc_realloc(void*, size_t);
void __libc_free(void*);
void* __libc_memalign(size_t, size_t);
}

struct AllocTracker {
  bool enabled = false;
  uint64_t alloc_count = 0;
  uint64_t free_count = 0;
  uint64_t bytes_allocated = 0;
  int64_t current_live_bytes = 0;
  int64_t peak_live_bytes = 0;

  void Reset() {
    alloc_count = 0;
    free_count = 0;
    bytes_allocated = 0;
    current_live_bytes = 0;
    peak_live_bytes = 0;
  }

  void RecordAlloc(size_t size) {
    if (!enabled) return;
    alloc_count++;
    bytes_allocated += size;
    current_live_bytes += size;
    if (current_live_bytes > peak_live_bytes) {
      peak_live_bytes = current_live_bytes;
    }
  }

  void RecordFree(size_t size) {
    if (!enabled) return;
    free_count++;
    current_live_bytes -= size;
    if (current_live_bytes < 0) current_live_bytes = 0;
  }
};

static thread_local AllocTracker g_tracker;

extern "C" void* malloc(size_t size) noexcept {
  void* ptr = __libc_malloc(size);
  if (ptr) {
    g_tracker.RecordAlloc(malloc_usable_size(ptr));
  }
  return ptr;
}

extern "C" void free(void* ptr) noexcept {
  if (ptr) {
    g_tracker.RecordFree(malloc_usable_size(ptr));
    __libc_free(ptr);
  }
}

extern "C" void* calloc(size_t nmemb, size_t size) noexcept {
  void* ptr = __libc_calloc(nmemb, size);
  if (ptr) {
    g_tracker.RecordAlloc(malloc_usable_size(ptr));
  }
  return ptr;
}

extern "C" void* realloc(void* ptr, size_t size) noexcept {
  if (ptr) {
    g_tracker.RecordFree(malloc_usable_size(ptr));
  }
  void* new_ptr = __libc_realloc(ptr, size);
  if (new_ptr) {
    g_tracker.RecordAlloc(malloc_usable_size(new_ptr));
  }
  return new_ptr;
}

extern "C" void* memalign(size_t alignment, size_t size) noexcept {
  void* ptr = __libc_memalign(alignment, size);
  if (ptr) {
    g_tracker.RecordAlloc(malloc_usable_size(ptr));
  }
  return ptr;
}

extern "C" void* aligned_alloc(size_t alignment, size_t size) noexcept {
  void* ptr = __libc_memalign(alignment, size);
  if (ptr) {
    g_tracker.RecordAlloc(malloc_usable_size(ptr));
  }
  return ptr;
}

extern "C" int posix_memalign(void** memptr, size_t alignment, size_t size) noexcept {
  void* ptr = __libc_memalign(alignment, size);
  if (!ptr && size != 0) return 12;  // ENOMEM
  if (ptr) {
    g_tracker.RecordAlloc(malloc_usable_size(ptr));
  }
  *memptr = ptr;
  return 0;
}

struct ScopedAllocTracker {
  explicit ScopedAllocTracker(bool enable = true) {
    g_tracker.Reset();
    g_tracker.enabled = enable;
  }
  ~ScopedAllocTracker() {
    g_tracker.enabled = false;
  }
  AllocTracker Snapshot() const {
    return g_tracker;
  }
};

// ============================================================================
// IAMF Bitstream Parser & Demuxer
// ============================================================================

struct IamfBitstream {
  std::vector<uint8_t> descriptors;
  std::vector<std::vector<uint8_t>> temporal_units;
};

static uint64_t ReadLeb128(const uint8_t* data, size_t size, size_t& offset) {
  uint64_t result = 0;
  int shift = 0;
  while (offset < size) {
    uint8_t byte = data[offset++];
    result |= static_cast<uint64_t>(byte & 0x7f) << shift;
    if ((byte & 0x80) == 0) break;
    shift += 7;
  }
  return result;
}

static bool SplitBitstream(const std::vector<uint8_t>& file_bytes, IamfBitstream& out) {
  size_t offset = 0;
  const size_t total_size = file_bytes.size();

  std::vector<uint8_t> cur_tu;
  std::vector<uint32_t> seen_substreams_in_tu;
  bool in_tu = false;
  bool seen_frames_in_tu = false;

  while (offset < total_size) {
    const size_t obu_start = offset;
    if (offset >= total_size) break;

    const uint8_t header_byte = file_bytes[offset++];
    const uint8_t obu_type = header_byte >> 3;

    size_t temp_offset = offset;
    const uint64_t obu_size = ReadLeb128(file_bytes.data(), total_size, temp_offset);
    offset = temp_offset;

    if (offset + obu_size > total_size) {
      std::cerr << "Truncated OBU at offset " << obu_start << "\n";
      return false;
    }

    const size_t obu_total_bytes = (offset + obu_size) - obu_start;
    const uint8_t* obu_ptr = file_bytes.data() + obu_start;

    // Check if this OBU belongs to descriptors
    const bool is_descriptor = (obu_type == 0 || obu_type == 1 || obu_type == 2 ||
                                obu_type == 24 || (obu_type >= 25 && obu_type <= 31));

    if (!in_tu && is_descriptor) {
      out.descriptors.insert(out.descriptors.end(), obu_ptr, obu_ptr + obu_total_bytes);
    } else {
      // Temporal Unit handling
      in_tu = true;

      // Determine substream id if AudioFrame
      uint32_t substream_id = 0;
      bool is_audio_frame = false;
      if (obu_type == 5) {
        is_audio_frame = true;
        size_t id_off = offset;
        substream_id = static_cast<uint32_t>(ReadLeb128(file_bytes.data(), total_size, id_off));
      } else if (obu_type >= 6 && obu_type <= 23) {
        is_audio_frame = true;
        substream_id = obu_type - 6;
      }

      // Check if this OBU marks the start of a new TU
      bool start_new_tu = false;
      if (obu_type == 4) {
        // Temporal delimiter always starts a new TU (except for very first empty TU)
        if (!cur_tu.empty()) start_new_tu = true;
      } else if (obu_type == 3 && seen_frames_in_tu) {
        // Parameter block after frames means new TU
        start_new_tu = true;
      } else if (is_audio_frame) {
        if (std::find(seen_substreams_in_tu.begin(), seen_substreams_in_tu.end(), substream_id) !=
            seen_substreams_in_tu.end()) {
          start_new_tu = true;
        }
      }

      if (start_new_tu && !cur_tu.empty()) {
        out.temporal_units.push_back(std::move(cur_tu));
        cur_tu.clear();
        seen_substreams_in_tu.clear();
        seen_frames_in_tu = false;
      }

      cur_tu.insert(cur_tu.end(), obu_ptr, obu_ptr + obu_total_bytes);
      if (is_audio_frame) {
        seen_substreams_in_tu.push_back(substream_id);
        seen_frames_in_tu = true;
      }
    }

    offset += obu_size;
  }

  if (!cur_tu.empty()) {
    out.temporal_units.push_back(std::move(cur_tu));
  }

  return !out.descriptors.empty() && !out.temporal_units.empty();
}

// ============================================================================
// Benchmark Execution Engine
// ============================================================================

struct DecodedOutput {
  std::vector<int32_t> samples;
  int num_channels = 0;
  uint32_t sample_rate = 0;
  uint32_t frame_size = 0;
  size_t total_temporal_units = 0;
};

struct StreamResult {
  std::string name;
  std::string layout_name;
  size_t tu_count = 0;
  double duration_sec = 0.0;
  int channels = 0;
  uint32_t sample_rate = 0;

  // Output shape agreement between the two decoders.
  bool shape_match = false;
  int rs_channels = 0;
  uint32_t rs_sample_rate = 0;
  size_t cpp_total_samples = 0;
  size_t rs_total_samples = 0;

  // Parity
  int64_t max_abs_lsb_diff_32b = 0;
  int64_t max_abs_diff_16b = 0;
  double percent_exact_16b = 100.0;
  double percent_exact_32b = 100.0;

  // Memory
  uint64_t cpp_init_allocs = 0;
  uint64_t cpp_init_bytes = 0;
  double cpp_allocs_per_tu = 0.0;
  double cpp_bytes_per_tu = 0.0;
  double cpp_peak_live_kb = 0.0;

  uint64_t rs_init_allocs = 0;
  uint64_t rs_init_bytes = 0;
  double rs_allocs_per_tu = 0.0;
  double rs_bytes_per_tu = 0.0;
  double rs_peak_live_kb = 0.0;

  // Throughput & Latency
  double cpp_create_us = 0.0;
  double rs_create_us = 0.0;
  double cpp_median_ms = 0.0;
  double cpp_realtime_x = 0.0;
  double cpp_melem_per_sec = 0.0;
  double cpp_tu_p50_us = 0.0;
  double cpp_tu_p99_us = 0.0;

  double rs_median_ms = 0.0;
  double rs_realtime_x = 0.0;
  double rs_melem_per_sec = 0.0;
  double rs_tu_p50_us = 0.0;
  double rs_tu_p99_us = 0.0;
};

static bool DecodeStream(
    iamf_tools::api::IamfDecoderInterface* decoder,
    const std::vector<std::vector<uint8_t>>& temporal_units,
    DecodedOutput& out,
    std::vector<uint8_t>& buffer,
    bool collect_samples,
    std::vector<double>* tu_latencies_us = nullptr) {
  if (collect_samples) {
    out.samples.clear();
  }
  (void)decoder->GetNumberOfOutputChannels(out.num_channels);
  (void)decoder->GetSampleRate(out.sample_rate);
  (void)decoder->GetFrameSize(out.frame_size);

  for (const auto& tu : temporal_units) {
    auto t0 = std::chrono::steady_clock::now();
    if (!decoder->Decode(tu.data(), tu.size()).ok()) return false;

    while (decoder->IsTemporalUnitAvailable()) {
      size_t written = 0;
      if (!decoder->GetOutputTemporalUnit(buffer.data(), buffer.size(), written).ok()) return false;
      if (collect_samples) {
        const size_t samples_in_unit = written / sizeof(int32_t);
        const int32_t* p = reinterpret_cast<const int32_t*>(buffer.data());
        out.samples.insert(out.samples.end(), p, p + samples_in_unit);
      }
    }
    auto t1 = std::chrono::steady_clock::now();
    if (tu_latencies_us) {
      tu_latencies_us->push_back(
          std::chrono::duration<double, std::micro>(t1 - t0).count());
    }
  }

  (void)decoder->SignalEndOfDecoding();
  while (decoder->IsTemporalUnitAvailable()) {
    size_t written = 0;
    if (!decoder->GetOutputTemporalUnit(buffer.data(), buffer.size(), written).ok()) return false;
    if (collect_samples) {
      const size_t samples_in_unit = written / sizeof(int32_t);
      const int32_t* p = reinterpret_cast<const int32_t*>(buffer.data());
      out.samples.insert(out.samples.end(), p, p + samples_in_unit);
    }
  }

  out.total_temporal_units = temporal_units.size();
  return true;
}

static void BenchmarkCase(
    const std::string& name,
    const std::string& path,
    iamf_tools::api::OutputLayout target_layout,
    const std::string& layout_name,
    int iters,
    StreamResult& res) {
  res.name = name;
  res.layout_name = layout_name;

  std::ifstream f(path, std::ios::binary);
  if (!f.is_open()) {
    std::cerr << "Failed to open " << path << "\n";
    return;
  }
  std::vector<uint8_t> bytes((std::istreambuf_iterator<char>(f)),
                             std::istreambuf_iterator<char>());
  f.close();

  IamfBitstream bitstream;
  if (!SplitBitstream(bytes, bitstream)) {
    std::cerr << "Failed to split bitstream " << path << "\n";
    return;
  }

  res.tu_count = bitstream.temporal_units.size();

  iamf_tools::api::IamfDecoderFactory::Settings settings;
  settings.requested_mix.output_layout = target_layout;
  settings.requested_output_sample_type =
      iamf_tools::api::OutputSampleType::kInt32LittleEndian;
  // Matches media/filters/iamf_audio_decoder.cc.
  settings.channel_ordering = iamf_tools::api::ChannelOrdering::kOrderingForAndroid;

  std::vector<uint8_t> scratch_buf(256 * 1024);

  // --------------------------------------------------------------------------
  // Pass 1: Parity & Bit-Exact Verification
  // --------------------------------------------------------------------------
  DecodedOutput cpp_out, rs_out;
  {
    auto cpp_dec = iamf_tools::api::IamfDecoderFactory::CreateFromDescriptors(
        settings, bitstream.descriptors.data(), bitstream.descriptors.size());
    auto rs_dec = iamf_rs::IamfRsDecoderAdapter::CreateFromDescriptors(
        settings, bitstream.descriptors.data(), bitstream.descriptors.size());

    if (!cpp_dec || !rs_dec) {
      std::cerr << name << ": Failed to create decoders (cpp=" << (cpp_dec != nullptr)
                << ", rs=" << (rs_dec != nullptr) << ")\n";
      return;
    }

    if (!DecodeStream(cpp_dec.get(), bitstream.temporal_units, cpp_out, scratch_buf, true)) {
      std::cerr << name << ": cpp decode failed\n";
      return;
    }
    if (!DecodeStream(rs_dec.get(), bitstream.temporal_units, rs_out, scratch_buf, true)) {
      std::cerr << name << ": rs decode failed\n";
      return;
    }
  }

  res.channels = cpp_out.num_channels;
  res.sample_rate = cpp_out.sample_rate;
  res.duration_sec = (cpp_out.num_channels > 0 && cpp_out.sample_rate > 0)
                         ? static_cast<double>(cpp_out.samples.size()) /
                               (cpp_out.num_channels * cpp_out.sample_rate)
                         : 0.0;

  res.rs_channels = rs_out.num_channels;
  res.rs_sample_rate = rs_out.sample_rate;
  res.cpp_total_samples = cpp_out.samples.size();
  res.rs_total_samples = rs_out.samples.size();
  res.shape_match = cpp_out.num_channels == rs_out.num_channels &&
                    cpp_out.sample_rate == rs_out.sample_rate &&
                    cpp_out.samples.size() == rs_out.samples.size();
  if (!res.shape_match) {
    std::cerr << name << ": output shape mismatch: cpp " << cpp_out.num_channels << "ch "
              << cpp_out.sample_rate << "Hz " << cpp_out.samples.size() << " samples, rs "
              << rs_out.num_channels << "ch " << rs_out.sample_rate << "Hz "
              << rs_out.samples.size() << " samples\n";
  }

  const size_t total_samples = std::min(cpp_out.samples.size(), rs_out.samples.size());
  int64_t max_diff_32b = 0;
  int64_t max_diff_16b = 0;
  size_t exact_32b = 0;
  size_t exact_16b = 0;

  for (size_t i = 0; i < total_samples; ++i) {
    int32_t s_cpp = cpp_out.samples[i];
    int32_t s_rs = rs_out.samples[i];
    int64_t diff32 = std::abs(static_cast<int64_t>(s_cpp) - static_cast<int64_t>(s_rs));
    if (diff32 > max_diff_32b) max_diff_32b = diff32;
    if (diff32 == 0) exact_32b++;

    int32_t s16_cpp = s_cpp >> 16;
    int32_t s16_rs = s_rs >> 16;
    int64_t diff16 = std::abs(static_cast<int64_t>(s16_cpp) - static_cast<int64_t>(s16_rs));
    if (diff16 > max_diff_16b) max_diff_16b = diff16;
    if (diff16 == 0) exact_16b++;
  }

  res.max_abs_lsb_diff_32b = max_diff_32b;
  res.max_abs_diff_16b = max_diff_16b;
  res.percent_exact_32b = total_samples > 0 ? (100.0 * exact_32b / total_samples) : 0.0;
  res.percent_exact_16b = total_samples > 0 ? (100.0 * exact_16b / total_samples) : 0.0;

  // --------------------------------------------------------------------------
  // Pass 2: Allocations Tracking
  // --------------------------------------------------------------------------
  {
    // C++ Allocations
    std::unique_ptr<iamf_tools::api::IamfDecoderInterface> cpp_dec;
    {
      ScopedAllocTracker tracker(true);
      cpp_dec = iamf_tools::api::IamfDecoderFactory::CreateFromDescriptors(
          settings, bitstream.descriptors.data(), bitstream.descriptors.size());
      auto snap = tracker.Snapshot();
      res.cpp_init_allocs = snap.alloc_count;
      res.cpp_init_bytes = snap.bytes_allocated;
    }
    {
      DecodedOutput dump;
      ScopedAllocTracker tracker(true);
      DecodeStream(cpp_dec.get(), bitstream.temporal_units, dump, scratch_buf, false);
      auto snap = tracker.Snapshot();
      res.cpp_allocs_per_tu = res.tu_count > 0 ? static_cast<double>(snap.alloc_count) / res.tu_count : 0.0;
      res.cpp_bytes_per_tu = res.tu_count > 0 ? static_cast<double>(snap.bytes_allocated) / res.tu_count : 0.0;
      res.cpp_peak_live_kb = static_cast<double>(snap.peak_live_bytes) / 1024.0;
    }

    // Rust Allocations
    std::unique_ptr<iamf_tools::api::IamfDecoderInterface> rs_dec;
    {
      ScopedAllocTracker tracker(true);
      rs_dec = iamf_rs::IamfRsDecoderAdapter::CreateFromDescriptors(
          settings, bitstream.descriptors.data(), bitstream.descriptors.size());
      auto snap = tracker.Snapshot();
      res.rs_init_allocs = snap.alloc_count;
      res.rs_init_bytes = snap.bytes_allocated;
    }
    {
      DecodedOutput dump;
      ScopedAllocTracker tracker(true);
      DecodeStream(rs_dec.get(), bitstream.temporal_units, dump, scratch_buf, false);
      auto snap = tracker.Snapshot();
      res.rs_allocs_per_tu = res.tu_count > 0 ? static_cast<double>(snap.alloc_count) / res.tu_count : 0.0;
      res.rs_bytes_per_tu = res.tu_count > 0 ? static_cast<double>(snap.bytes_allocated) / res.tu_count : 0.0;
      res.rs_peak_live_kb = static_cast<double>(snap.peak_live_bytes) / 1024.0;
    }
  }

  // --------------------------------------------------------------------------
  // Pass 3: Throughput & Latency Timing
  // --------------------------------------------------------------------------
  const int warmup_iters = (res.duration_sec > 5.0) ? 1 : 3;
  const int bench_iters = (res.duration_sec > 10.0) ? 3 : iters;

  // The two decoders alternate which runs first on each iteration so slow
  // periods on a shared machine do not land on one side only.
  struct Timing {
    std::vector<double> create_us;
    std::vector<double> stream_ms;
    std::vector<double> tu_us;
  };
  Timing cpp_t, rs_t;
  auto run_once = [&](bool rust, bool record) {
    Timing& t = rust ? rs_t : cpp_t;
    const auto c0 = std::chrono::steady_clock::now();
    std::unique_ptr<iamf_tools::api::IamfDecoderInterface> dec =
        rust ? iamf_rs::IamfRsDecoderAdapter::CreateFromDescriptors(
                   settings, bitstream.descriptors.data(), bitstream.descriptors.size())
             : iamf_tools::api::IamfDecoderFactory::CreateFromDescriptors(
                   settings, bitstream.descriptors.data(), bitstream.descriptors.size());
    const auto c1 = std::chrono::steady_clock::now();
    DecodedOutput dump;
    std::vector<double> tu_lats;
    const auto t0 = std::chrono::steady_clock::now();
    DecodeStream(dec.get(), bitstream.temporal_units, dump, scratch_buf, false, &tu_lats);
    const auto t1 = std::chrono::steady_clock::now();
    if (record) {
      t.create_us.push_back(std::chrono::duration<double, std::micro>(c1 - c0).count());
      t.stream_ms.push_back(std::chrono::duration<double, std::milli>(t1 - t0).count());
      t.tu_us.insert(t.tu_us.end(), tu_lats.begin(), tu_lats.end());
    }
  };
  for (int it = 0; it < warmup_iters + bench_iters; ++it) {
    const bool record = it >= warmup_iters;
    const bool rust_first = (it % 2) == 1;
    run_once(rust_first, record);
    run_once(!rust_first, record);
  }
  auto median = [](std::vector<double> v) {
    std::sort(v.begin(), v.end());
    return v[v.size() / 2];
  };
  auto pct = [](std::vector<double> v, int p) {
    std::sort(v.begin(), v.end());
    return v[v.size() * p / 100];
  };
  res.cpp_create_us = median(cpp_t.create_us);
  res.rs_create_us = median(rs_t.create_us);
  res.cpp_median_ms = median(cpp_t.stream_ms);
  res.rs_median_ms = median(rs_t.stream_ms);
  res.cpp_realtime_x = res.duration_sec * 1000.0 / res.cpp_median_ms;
  res.rs_realtime_x = res.duration_sec * 1000.0 / res.rs_median_ms;
  res.cpp_melem_per_sec = total_samples / (res.cpp_median_ms * 1000.0);
  res.rs_melem_per_sec = total_samples / (res.rs_median_ms * 1000.0);
  res.cpp_tu_p50_us = pct(cpp_t.tu_us, 50);
  res.cpp_tu_p99_us = pct(cpp_t.tu_us, 99);
  res.rs_tu_p50_us = pct(rs_t.tu_us, 50);
  res.rs_tu_p99_us = pct(rs_t.tu_us, 99);
}

// ============================================================================
// Markdown Reporter
// ============================================================================

static void PrintMarkdownReport(const std::vector<StreamResult>& results) {
  std::cout << "\n# iamf-tools (C++) vs iamf-rs (Rust), in-process\n\n";

  std::cout << "## Output parity (iamf-rs vs iamf-tools)\n\n";
  std::cout << "| Stream | Layout | Ch | Rate | Chunks | Duration | Same shape | "
               "16-bit exact | Max 16-bit diff | Max 32-bit diff |\n";
  std::cout << "| :--- | :--- | ---: | ---: | ---: | ---: | :---: | ---: | ---: | ---: |\n";
  for (const auto& r : results) {
    std::cout << "| " << r.name << " | " << r.layout_name << " | " << r.channels << " | "
              << r.sample_rate << " | " << r.tu_count << " | " << std::fixed
              << std::setprecision(2) << r.duration_sec << " s | "
              << (r.shape_match ? "yes" : "NO") << " | " << r.percent_exact_16b << "% | "
              << r.max_abs_diff_16b << " | " << r.max_abs_lsb_diff_32b << " |\n";
  }

  std::cout << "\n## Heap allocations (glibc malloc interposition, calling thread)\n\n";
  std::cout << "| Stream | C++ init allocs | Rust init allocs | C++ init KB | Rust init KB | "
               "C++ allocs/chunk | Rust allocs/chunk | C++ B/chunk | Rust B/chunk | "
               "C++ peak growth KB | Rust peak growth KB |\n";
  std::cout << "| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n";
  for (const auto& r : results) {
    std::cout << "| " << r.name << " | " << r.cpp_init_allocs << " | " << r.rs_init_allocs
              << " | " << std::fixed << std::setprecision(1) << r.cpp_init_bytes / 1024.0
              << " | " << r.rs_init_bytes / 1024.0 << " | " << r.cpp_allocs_per_tu << " | "
              << r.rs_allocs_per_tu << " | " << std::setprecision(0) << r.cpp_bytes_per_tu
              << " | " << r.rs_bytes_per_tu << " | " << std::setprecision(1)
              << r.cpp_peak_live_kb << " | " << r.rs_peak_live_kb << " |\n";
  }

  std::cout << "\n## Decode time (median of timed runs)\n\n";
  std::cout << "| Stream | C++ ms | Rust ms | C++ x realtime | Rust x realtime | C++ / Rust | "
               "C++ create us | Rust create us |\n";
  std::cout << "| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |\n";
  for (const auto& r : results) {
    const double ratio = (r.rs_median_ms > 0) ? (r.cpp_median_ms / r.rs_median_ms) : 0.0;
    std::cout << "| " << r.name << " | " << std::fixed << std::setprecision(2)
              << r.cpp_median_ms << " | " << r.rs_median_ms << " | " << std::setprecision(1)
              << r.cpp_realtime_x << " | " << r.rs_realtime_x << " | " << std::setprecision(2)
              << ratio << " | " << std::setprecision(0) << r.cpp_create_us << " | "
              << r.rs_create_us << " |\n";
  }

  std::cout << "\n## Per-chunk latency (Decode + drain, all timed runs pooled)\n\n";
  std::cout << "| Stream | C++ p50 us | Rust p50 us | C++ p99 us | Rust p99 us |\n";
  std::cout << "| :--- | ---: | ---: | ---: | ---: |\n";
  for (const auto& r : results) {
    std::cout << "| " << r.name << " | " << std::fixed << std::setprecision(1)
              << r.cpp_tu_p50_us << " | " << r.rs_tu_p50_us << " | " << r.cpp_tu_p99_us
              << " | " << r.rs_tu_p99_us << " |\n";
  }

  // Machine-readable lines for aggregating repeated runs.
  std::cout << "\n" << std::defaultfloat << std::setprecision(10);
  for (const auto& r : results) {
    std::cout << "RESULT\t" << r.name << "\t" << r.shape_match << "\t" << r.percent_exact_16b
              << "\t" << r.max_abs_diff_16b << "\t" << r.max_abs_lsb_diff_32b << "\t"
              << r.cpp_init_allocs << "\t" << r.rs_init_allocs << "\t" << r.cpp_init_bytes
              << "\t" << r.rs_init_bytes << "\t" << r.cpp_allocs_per_tu << "\t"
              << r.rs_allocs_per_tu << "\t" << r.cpp_bytes_per_tu << "\t" << r.rs_bytes_per_tu
              << "\t" << r.cpp_peak_live_kb << "\t" << r.rs_peak_live_kb << "\t"
              << r.cpp_median_ms << "\t" << r.rs_median_ms << "\t" << r.cpp_tu_p50_us << "\t"
              << r.rs_tu_p50_us << "\t" << r.cpp_tu_p99_us << "\t" << r.rs_tu_p99_us << "\t"
              << r.cpp_create_us << "\t" << r.rs_create_us << "\n";
  }
}

int main(int argc, char** argv) {
  // Keeps iamf-tools' log output off stderr. Log statements that are compiled
  // in still construct a message (and allocate) before being dropped; build
  // with --strip-logs to compile them out.
  absl::InitializeLog();
  absl::SetStderrThreshold(absl::LogSeverityAtLeast::kInfinity);
  absl::SetMinLogLevel(absl::LogSeverityAtLeast::kInfinity);

  std::string base_dir = ".";
  if (argc > 1) {
    base_dir = argv[1];
  }

  struct TestCase {
    std::string name;
    std::string rel_path;
    iamf_tools::api::OutputLayout layout;
    std::string layout_name;
    int iters;
  };

  const std::vector<TestCase> cases = {
      {"test_000002 (LPCM stereo)", "tests/vectors/test_000002.iamf",
       iamf_tools::api::OutputLayout::kItu2051_SoundSystemA_0_2_0, "Stereo", 10},
      {"test_000026 (Opus stereo)", "tests/vectors/test_000026.iamf",
       iamf_tools::api::OutputLayout::kItu2051_SoundSystemA_0_2_0, "Stereo", 10},
      {"test_000036 (LPCM 2-layer scalable)", "tests/vectors/test_000036.iamf",
       iamf_tools::api::OutputLayout::kItu2051_SoundSystemB_0_5_0, "5.1", 10},
      {"test_000038 (LPCM FOA)", "tests/vectors/test_000038.iamf",
       iamf_tools::api::OutputLayout::kItu2051_SoundSystemA_0_2_0, "Stereo", 10},
      {"test_000070 (LPCM 7.1.4)", "tests/vectors/test_000070.iamf",
       iamf_tools::api::OutputLayout::kItu2051_SoundSystemJ_4_7_0, "7.1.4", 10},
      {"test_000086 (LPCM FOA + 2-layer)", "tests/vectors/test_000086.iamf",
       iamf_tools::api::OutputLayout::kItu2051_SoundSystemA_0_2_0, "Stereo", 10},
      {"7_1_4_PCM16_48000 (demo, LPCM 7.1.4)", "tests/demo/7_1_4_PCM16_48000.iamf",
       iamf_tools::api::OutputLayout::kItu2051_SoundSystemJ_4_7_0, "7.1.4", 5},
      {"7_1_4_Opus (demo, Opus 7.1.4)", "tests/demo/7_1_4_Opus.iamf",
       iamf_tools::api::OutputLayout::kItu2051_SoundSystemJ_4_7_0, "7.1.4", 5},
      {"Animated_demo_3OA (demo, Opus 3OA)", "tests/demo/Animated_demo_3OA.iamf",
       iamf_tools::api::OutputLayout::kItu2051_SoundSystemA_0_2_0, "Stereo", 5},
      {"Animated_demo_3OA_and_2_0 (demo, Opus 3OA + stereo)",
       "tests/demo/Animated_demo_3OA_and_2_0.iamf",
       iamf_tools::api::OutputLayout::kItu2051_SoundSystemA_0_2_0, "Stereo", 5},
  };

  std::vector<StreamResult> results;
  for (const auto& c : cases) {
    const std::string full_path = base_dir + "/" + c.rel_path;
    std::cout << "Benchmarking " << c.name << " (" << full_path << ")..." << std::endl;
    StreamResult res;
    BenchmarkCase(c.name, full_path, c.layout, c.layout_name, c.iters, res);
    results.push_back(res);
  }

  PrintMarkdownReport(results);
  return 0;
}
