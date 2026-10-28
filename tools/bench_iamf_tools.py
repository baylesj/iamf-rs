#!/usr/bin/env python3
"""Builds and runs tools/bench_iamf_tools.cc.

Compiles the iamf-tools decoder sources listed in Chromium's
third_party/iamf_tools/BUILD.gn (with the same feature defines), plus Abseil,
from a Chromium checkout, links them with the iamf-capi static library built
with Chromium's feature set (pcm, opus-ffi), and runs the benchmark against
tests/vectors and tests/demo (populate with tools/fetch_vectors.sh --all and
--demo). Both decoders link the system libopus.

Like Chromium's release build, iamf-tools is compiled with strip_errors.h
force-included. Chromium does not compile out absl log statements; pass
--strip-logs to build iamf-tools with ABSL_MIN_LOG_LEVEL=99 so that log
statements cost nothing.

Usage:
  CHROMIUM_SRC=/path/to/chromium/src python3 tools/bench_iamf_tools.py [--strip-logs]
"""

import os
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor

REPO_ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), ".."))
CHROMIUM_SRC = os.environ.get("CHROMIUM_SRC")
if not CHROMIUM_SRC:
    sys.exit("Set CHROMIUM_SRC to a Chromium src/ checkout (for iamf_tools and abseil-cpp).")
THIRD_PARTY_DIR = os.path.join(CHROMIUM_SRC, "third_party")
IAMF_TOOLS_DIR = os.path.join(THIRD_PARTY_DIR, "iamf_tools")
ABSEIL_DIR = os.path.join(THIRD_PARTY_DIR, "abseil-cpp")
OPUS_DIR = os.path.join(THIRD_PARTY_DIR, "opus", "src")

STRIP_LOGS = "--strip-logs" in sys.argv[1:]
BUILD_DIR = os.path.join(REPO_ROOT, "target", "bench-iamf-tools")
OBJ_DIR = os.path.join(BUILD_DIR, "obj")
# iamf-tools objects and the binary depend on the variant; Abseil does not.
VARIANT = "strip-logs" if STRIP_LOGS else "default"
VARIANT_DIR = os.path.join(BUILD_DIR, VARIANT)
BIN_PATH = os.path.join(VARIANT_DIR, "bench_iamf_tools")

# Decoder sources from Chromium's third_party/iamf_tools/BUILD.gn.
IAMF_TOOLS_SOURCES = [
    "src/iamf/api/conversion/channel_reorderer.cc",
    "src/iamf/api/conversion/mix_presentation_conversion.cc",
    "src/iamf/api/conversion/profile_conversion.cc",
    "src/iamf/api/decoder/iamf_decoder.cc",
    "src/iamf/cli/audio_frame_decoder.cc",
    "src/iamf/cli/channel_label.cc",
    "src/iamf/cli/cli_util.cc",
    "src/iamf/cli/codec/lpcm_decoder.cc",
    "src/iamf/cli/codec/opus_decoder.cc",
    "src/iamf/cli/codec/opus_utils.cc",
    "src/iamf/cli/demixer.cc",
    "src/iamf/cli/demixing_manager.cc",
    "src/iamf/cli/descriptor_obu_parser.cc",
    "src/iamf/cli/global_timing_module.cc",
    "src/iamf/cli/labeled_frame.cc",
    "src/iamf/cli/layout_renderer_factory.cc",
    "src/iamf/cli/loudness_calculator_base.cc",
    "src/iamf/cli/loudness_calculator_factory_base.cc",
    "src/iamf/cli/obu_processor.cc",
    "src/iamf/cli/obu_processor_utils.cc",
    "src/iamf/cli/obu_with_data_generator.cc",
    "src/iamf/cli/parameters_manager.cc",
    "src/iamf/cli/profile_filter.cc",
    "src/iamf/cli/recon_gain_generator.cc",
    "src/iamf/cli/renderer/audio_element_renderer_ambisonics_to_channel.cc",
    "src/iamf/cli/renderer/audio_element_renderer_base.cc",
    "src/iamf/cli/renderer/audio_element_renderer_channel_to_channel.cc",
    "src/iamf/cli/renderer/audio_element_renderer_passthrough.cc",
    "src/iamf/cli/renderer/default_layout_renderer.cc",
    "src/iamf/cli/renderer/gains/get_gains.cc",
    "src/iamf/cli/renderer/gains/precomputed_compressed_gains.cc",
    "src/iamf/cli/renderer/gains/precomputed_compressed_gains_decoder.cc",
    "src/iamf/cli/renderer/layout_renderer_base.cc",
    "src/iamf/cli/renderer/loudspeakers_renderer.cc",
    "src/iamf/cli/renderer/renderer_utils.cc",
    "src/iamf/cli/renderer_factory.cc",
    "src/iamf/cli/rendering_mix_presentation_finalizer.cc",
    "src/iamf/cli/sample_processing_utils.cc",
    "src/iamf/cli/sample_processor_base.cc",
    "src/iamf/cli/temporal_unit_view.cc",
    "src/iamf/common/leb_generator.cc",
    "src/iamf/common/q_format_or_floating_point.cc",
    "src/iamf/common/read_bit_buffer.cc",
    "src/iamf/common/utils/numeric_utils.cc",
    "src/iamf/common/write_bit_buffer.cc",
    "src/iamf/include/iamf_tools/iamf_decoder_factory.cc",
    "src/iamf/include/iamf_tools/iamf_tools_api_types.cc",
    "src/iamf/obu/ambisonics_config.cc",
    "src/iamf/obu/arbitrary_obu.cc",
    "src/iamf/obu/audio_element.cc",
    "src/iamf/obu/audio_frame.cc",
    "src/iamf/obu/cartesian_position_data.cc",
    "src/iamf/obu/codec_config.cc",
    "src/iamf/obu/decoder_config/aac_decoder_config.cc",
    "src/iamf/obu/decoder_config/flac_decoder_config.cc",
    "src/iamf/obu/decoder_config/lpcm_decoder_config.cc",
    "src/iamf/obu/decoder_config/opus_decoder_config.cc",
    "src/iamf/obu/demixing_info_parameter_data.cc",
    "src/iamf/obu/element_gain_offset_config.cc",
    "src/iamf/obu/extension_parameter_data.cc",
    "src/iamf/obu/ia_sequence_header.cc",
    "src/iamf/obu/metadata_obu.cc",
    "src/iamf/obu/mix_gain_parameter_data.cc",
    "src/iamf/obu/mix_presentation.cc",
    "src/iamf/obu/obu_base.cc",
    "src/iamf/obu/obu_header.cc",
    "src/iamf/obu/param_definitions/cart16_param_definition.cc",
    "src/iamf/obu/param_definitions/cart8_param_definition.cc",
    "src/iamf/obu/param_definitions/demixing_param_definition.cc",
    "src/iamf/obu/param_definitions/dual_cart16_param_definition.cc",
    "src/iamf/obu/param_definitions/dual_cart8_param_definition.cc",
    "src/iamf/obu/param_definitions/dual_polar_param_definition.cc",
    "src/iamf/obu/param_definitions/extended_param_definition.cc",
    "src/iamf/obu/param_definitions/mix_gain_param_definition.cc",
    "src/iamf/obu/param_definitions/param_definition_base.cc",
    "src/iamf/obu/param_definitions/polar_param_definition.cc",
    "src/iamf/obu/param_definitions/recon_gain_param_definition.cc",
    "src/iamf/obu/param_definitions/subblock_schedule.cc",
    "src/iamf/obu/parameter_block.cc",
    "src/iamf/obu/polar_position_data.cc",
    "src/iamf/obu/recon_gain_info_parameter_data.cc",
    "src/iamf/obu/rendering_config.cc",
    "src/iamf/obu/substream_channel_count.cc",
]

def run(cmd, cwd=REPO_ROOT):
    subprocess.check_call(cmd, cwd=cwd)

def compile_file(args):
    src, dst, incs = args[:3]
    extra = args[3] if len(args) > 3 else []
    if os.path.exists(dst) and os.path.getmtime(dst) > os.path.getmtime(src):
        return
    os.makedirs(os.path.dirname(dst), exist_ok=True)
    cmd = [
        "/usr/bin/clang++",
        "-c",
        "-O3",
        "-DNDEBUG",
        "-std=c++20",
        "-fPIC",
        "-DIAMF_TOOLS_DISABLE_AAC_DECODER",
        "-DIAMF_TOOLS_DISABLE_FLAC_DECODER",
        "-DIAMF_TOOLS_DISABLE_BINAURAL_RENDERING",
        "-Wall",
        "-Wno-deprecated-declarations",
        "-Wno-unused-variable",
        "-Wno-unused-function",
        "-Wno-sign-compare",
        "-Wno-defaulted-function-deleted",
    ]
    for inc in incs:
        cmd.extend(["-I", inc])
    cmd.extend(extra)
    cmd.extend([src, "-o", dst])
    subprocess.check_call(cmd)

def main():
    print("=== Step 1: Build Rust iamf-capi release staticlib ===")
    cargo_cmd = [
        "cargo", "build", "--release",
        "--manifest-path", os.path.join(REPO_ROOT, "Cargo.toml"),
        "-p", "iamf-capi",
        "--no-default-features",
        "--features", "pcm,opus-ffi"
    ]
    run(cargo_cmd)
    rs_lib = os.path.join(REPO_ROOT, "target", "release", "libiamf_rs.a")
    assert os.path.exists(rs_lib), f"Missing {rs_lib}"

    os.makedirs(OBJ_DIR, exist_ok=True)

    common_incs = [
        IAMF_TOOLS_DIR,
        os.path.join(IAMF_TOOLS_DIR, "src"),
        ABSEIL_DIR,
        OPUS_DIR,
        "/usr/include/opus",
        os.path.join(REPO_ROOT, "crates", "iamf-capi", "include"),
        os.path.join(REPO_ROOT, "examples", "chromium"),
        REPO_ROOT,
    ]

    print("=== Step 2: Compile Abseil C++ Sources ===")
    absl_tasks = []
    absl_objs = []
    for root, _, files in os.walk(os.path.join(ABSEIL_DIR, "absl")):
        for f in files:
            if (f.endswith(".cc") and
                "test" not in f and
                "benchmark" not in f and
                "mock" not in f and
                "matcher" not in f and
                not f.endswith("_win.cc") and
                not f.endswith("_windows.cc")):
                src = os.path.join(root, f)
                rel = os.path.relpath(src, ABSEIL_DIR)
                dst = os.path.join(OBJ_DIR, "absl", rel.replace(".cc", ".o"))
                absl_tasks.append((src, dst, [ABSEIL_DIR]))
                absl_objs.append(dst)

    with ThreadPoolExecutor(max_workers=32) as ex:
        list(ex.map(compile_file, absl_tasks))

    libabsl_path = os.path.join(BUILD_DIR, "libabsl.a")
    if not os.path.exists(libabsl_path):
        print(f"Archiving {libabsl_path}...")
        # Use rspfile or chunked ar
        rsp = os.path.join(BUILD_DIR, "absl_objs.rsp")
        with open(rsp, "w") as f:
            f.write("\n".join(absl_objs))
        run(["ar", "rcs", libabsl_path, f"@{rsp}"])

    print("=== Step 3: Compile iamf_tools C++ Sources ===")
    iamf_flags = ["-include", os.path.join(IAMF_TOOLS_DIR, "strip_errors.h")]
    if STRIP_LOGS:
        iamf_flags.append("-DABSL_MIN_LOG_LEVEL=99")
    iamf_tasks = []
    iamf_objs = []
    for rel in IAMF_TOOLS_SOURCES:
        src = os.path.join(IAMF_TOOLS_DIR, rel)
        dst = os.path.join(VARIANT_DIR, "obj", rel.replace(".cc", ".o"))
        iamf_tasks.append((src, dst, common_incs, iamf_flags))
        iamf_objs.append(dst)

    with ThreadPoolExecutor(max_workers=32) as ex:
        list(ex.map(compile_file, iamf_tasks))

    libiamf_path = os.path.join(VARIANT_DIR, "libiamf_tools.a")
    print(f"Archiving {libiamf_path}...")
    rsp = os.path.join(VARIANT_DIR, "iamf_objs.rsp")
    with open(rsp, "w") as f:
        f.write("\n".join(iamf_objs))
    run(["ar", "rcs", libiamf_path, f"@{rsp}"])

    print("=== Step 4: Compile Adapter and Benchmark Binary ===")
    adapter_cc = os.path.join(REPO_ROOT, "examples", "chromium", "iamf_rs_decoder_adapter.cc")
    adapter_o = os.path.join(OBJ_DIR, "iamf_rs_decoder_adapter.o")
    compile_file((adapter_cc, adapter_o, common_incs))

    bench_cc = os.path.join(REPO_ROOT, "tools", "bench_iamf_tools.cc")
    bench_o = os.path.join(VARIANT_DIR, "bench_iamf_tools.o")
    compile_file((bench_cc, bench_o, common_incs))

    print("=== Step 5: Linking bench_iamf_tools ===")
    link_cmd = [
        "/usr/bin/clang++",
        "-O3",
        "-std=c++20",
        bench_o,
        adapter_o,
        "-Wl,--whole-archive",
        libiamf_path,
        "-Wl,--no-whole-archive",
        rs_lib,
        libabsl_path,
        "-lopus",
        "-lpthread",
        "-ldl",
        "-lm",
        "-o",
        BIN_PATH,
    ]
    run(link_cmd)
    print(f"Successfully built {BIN_PATH}")

    print(f"=== Step 6: Running benchmark ({VARIANT}) ===")
    run([BIN_PATH, REPO_ROOT])

if __name__ == "__main__":
    main()
