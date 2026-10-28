# iamf-rs vs. iamf-tools: output, speed, and memory (Oct 2026)

Chromium decodes IAMF with its copy of iamf-tools (v3.0.0). This note compares
iamf-rs against it. Both decoders run through the decoder interface that
Chromium's `IamfAudioDecoder` uses.

We tested 10 streams. On every one, iamf-rs gave the same output shape as
iamf-tools, and its samples were within 1 LSB at 16 bits. iamf-rs was faster
on all 10: 4.6x to 7.8x on LPCM streams and 1.3x to 2.0x on Opus streams. It
made 5x to 8x fewer heap allocations when creating a decoder and about 2x to
3x fewer per temporal unit. It did allocate more bytes in some cases: 7% to 8%
more at creation on the three multichannel Opus streams, and more per temporal
unit on two Opus streams.

These numbers come from one machine, outside Chromium's build, on a small set
of streams. Read [Limits](#limits) before you quote them.

## Setup

The harness (`tools/bench_iamf_tools.cc`, built and run by
`tools/bench_iamf_tools.py`) links both decoders into one process.

- iamf-tools: the decoder sources listed in Chromium's
  `third_party/iamf_tools/BUILD.gn`, with the same feature defines (AAC, FLAC,
  and binaural disabled) and the `strip_errors.h` force-include that Chromium
  uses in release builds.
- iamf-rs: `iamf-capi` with Chromium's feature set (`pcm`, `opus-ffi`), wrapped
  by `examples/chromium/iamf_rs_decoder_adapter.cc`.
- Both get the settings Chromium passes: a requested output layout, 32-bit
  integer output, and `kOrderingForAndroid` channel order.
- Both link the same system libopus (1.6.1).
- The harness splits each file into the descriptor OBUs and one buffer per
  temporal unit (TU), using its own OBU walker. The TU counts match the stream
  duration divided by the frame size, rounded up. Both decoders get the same
  buffers.

Each stream goes through three passes:

1. Parity: decode with both, then compare channel count, sample rate, sample
   count, and every sample.
2. Allocations: count calls to `malloc` and related functions during decoder
   creation and during decode, by replacing the glibc entry points. Sizes are
   the usable sizes that glibc reports.
3. Timing: time decoder creation and the full decode, with allocation counting
   off. After 1 to 3 warmup runs, streams up to 10 s get 10 timed runs and
   longer streams get 3. The two decoders alternate which one runs first. The
   tables report the median.

Machine: AMD EPYC 7B13 (128 threads, shared, not pinned), clang 21.1.8 at
`-O3` with libstdc++, rustc 1.93.1 with the default release profile. Neither
side used LTO.

### Logging in iamf-tools

iamf-tools has many `ABSL_LOG(INFO)` statements on the decoder creation path,
and some on the first frame of decode. Even when nothing is printed, each one
that runs builds a log message, and in our measurement each added 26 to 27 KiB
of heap allocation. Logging added 1.3 MiB to 1.9 MiB per decoder creation.

We did not find `ABSL_MIN_LOG_LEVEL` set in Chromium's build files, so Chrome
may pay this cost too. We have not confirmed that in a Chrome build. To keep
the comparison about decoder work, all numbers below use an iamf-tools build
with `ABSL_MIN_LOG_LEVEL=99` (`--strip-logs`), which compiles the log
statements out. This favors iamf-tools.

## Output parity

This compares iamf-rs with iamf-tools, not with the libiamf reference renders.

| Stream | Output | 16-bit identical | Max diff, 16-bit | Max diff, 32-bit |
| --- | --- | ---: | ---: | ---: |
| test_000002, LPCM stereo | stereo | 100% | 0 | 0 |
| test_000026, Opus stereo | stereo | 100% | 0 | 1 |
| test_000036, LPCM 2-layer scalable | 5.1 | 99.98% | 1 | 95 |
| test_000038, LPCM first-order ambisonics | stereo | 100% | 0 | 181 |
| test_000070, LPCM 7.1.4 | 7.1.4 | 100% | 0 | 0 |
| test_000086, LPCM ambisonics + 2-layer | stereo | 93.26% | 1 | 43254 |
| demo 7_1_4_PCM16_48000, LPCM 7.1.4 | 7.1.4 | 100% | 0 | 0 |
| demo 7_1_4_Opus, Opus 7.1.4 | 7.1.4 | 99.9999% | 1 | 1 |
| demo Animated_demo_3OA, Opus 3rd-order ambisonics | stereo | 99.95% | 1 | 1293 |
| demo Animated_demo_3OA_and_2_0, Opus 3OA + stereo | stereo | 99.95% | 1 | 1264 |

Channel count, sample rate, and sample count matched on all 10 streams in all
runs. Three streams are identical at full 32-bit output.

Chromium uses the 32-bit samples. The 16-bit columns take the top 16 bits of
each sample, so a 32-bit difference of 1 can show up as a 16-bit difference of
1, as on the Opus 7.1.4 demo. The largest difference, 43254 on test_000086, is
about 0.66 of a 16-bit LSB, or about -94 dB below full scale. We have not
traced where the differences come from.

## Speed

Median of 3 harness runs. The ratio is iamf-tools time divided by iamf-rs time,
shown as the range across the 3 runs.

| Stream | Audio | iamf-tools decode | iamf-rs decode | Ratio | Create, C++ / Rust |
| --- | ---: | ---: | ---: | ---: | ---: |
| test_000002, LPCM stereo | 0.5 s | 1.55 ms | 0.28 ms | 4.7 to 5.5 | 21 / 3 us |
| test_000026, Opus stereo | 0.5 s | 2.91 ms | 1.47 ms | 1.9 to 2.0 | 135 / 109 us |
| test_000036, LPCM 2-layer scalable | 5 s | 98.8 ms | 18.0 ms | 5.5 | 70 / 15 us |
| test_000038, LPCM ambisonics | 0.5 s | 8.50 ms | 1.33 ms | 6.3 to 6.4 | 29 / 5 us |
| test_000070, LPCM 7.1.4 | 5 s | 166 ms | 36.4 ms | 4.6 | 81 / 17 us |
| test_000086, LPCM ambisonics + 2-layer | 5 s | 200 ms | 25.7 ms | 7.7 to 7.8 | 106 / 22 us |
| demo LPCM 7.1.4 | 25 s | 833 ms | 143 ms | 5.8 | 82 / 13 us |
| demo Opus 7.1.4 | 25 s | 342 ms | 266 ms | 1.3 | 935 / 808 us |
| demo Opus 3OA | 99 s | 2593 ms | 2023 ms | 1.3 | 1736 / 1664 us |
| demo Opus 3OA + stereo | 99 s | 2790 ms | 2166 ms | 1.3 | 1913 / 1822 us |

Both decoders call the same libopus, so on Opus streams much of the time is
probably shared. That would explain the smaller gap, but we have not profiled
either decoder to confirm it, or to find where the LPCM gap comes from.

Per TU, iamf-rs had lower p50 and p99 latency on every stream in every run. The
closest case was the Opus 7.1.4 demo, where iamf-tools p99 was 1.24x iamf-rs
p99 in one run.

## Heap allocations

Numbers are from run 1. Allocation counts were the same in all 3 runs. KiB is
1024 bytes.

| Stream | Allocs at creation, C++ / Rust | KiB at creation, C++ / Rust | Allocs per TU, C++ / Rust | KiB per TU, C++ / Rust |
| --- | ---: | ---: | ---: | ---: |
| test_000002, LPCM stereo | 166 / 30 | 16 / 5 | 41 / 21 | 9.6 / 8.8 |
| test_000026, Opus stereo | 168 / 32 | 245 / 246 | 42 / 21 | 36.8 / 60.2 |
| test_000036, LPCM 2-layer | 256 / 36 | 50 / 6 | 115 / 47 | 122 / 94 |
| test_000038, LPCM ambisonics | 220 / 33 | 18 / 5 | 61 / 28 | 10.7 / 5.5 |
| test_000070, LPCM 7.1.4 | 312 / 41 | 133 / 6 | 148 / 75 | 344 / 347 |
| test_000086, LPCM ambisonics + 2-layer | 434 / 56 | 34 / 7 | 178 / 59 | 33.6 / 12.7 |
| demo LPCM 7.1.4 | 315 / 41 | 133 / 6 | 148 / 75 | 343 / 347 |
| demo Opus 7.1.4 | 328 / 55 | 1523 / 1629 | 148 / 75 | 188 / 325 |
| demo Opus 3OA | 399 / 74 | 3116 / 3361 | 188 / 66 | 263 / 160 |
| demo Opus 3OA + stereo | 529 / 95 | 3334 / 3602 | 217 / 79 | 295 / 194 |

"KiB" is the total size of all allocations made, not memory held at one time.
iamf-rs makes fewer allocations on every stream. It allocates 7% to 8% more
bytes at creation on the three multichannel Opus streams, more bytes per TU on
Opus stereo and Opus 7.1.4, and about the same per TU on LPCM 7.1.4. These are
places to look next.

The harness also records the peak growth in live heap during decode. iamf-rs
peaked 2.0x to 3.3x lower (for example 247 KiB vs. 537 KiB on LPCM 7.1.4).
This leaves out memory allocated at creation, so it is not total memory use.

## Conformance coverage

For this work, `tests/vectors` held all 333 libiamf test vectors and their 573
reference renders (560 loudspeaker, 13 binaural), copied from a libiamf
checkout. `tests/demo` held the 5 iamf-tools web demo streams.

The parse test (`crates/iamf-obu/tests/vectors.rs`) checks OBU framing and
descriptor parsing, not decoded output. It sorts vectors by their metadata:

- 306 strictly valid vectors parse with no errors and contain every
  descriptor type.
- 9 vectors are invalid but still decodable. They contain at least one of each
  descriptor type, and the test allows a malformed descriptor among them.
- 18 vectors must fail to decode. The test only checks that parsing them does
  not panic.

Decoded output is compared against reference renders in 19 cases in
`tools/iamfdec/tests/render.rs`. Most use 0 or 1 LSB tolerance. The only Opus
case (test_000048) allows 3000 LSB, because its reference is rendered from the
audio before Opus coding. Comparing against all reference renders is the next
step.

Running the full set found four gaps. iamf-rs handles them as follows:

- OBU types 25 to 30 are reserved. iamf-rs skips them instead of failing
  (test_000116).
- The OBU header bit that means "trimming" on audio frames has a different
  meaning on other OBU types (for example, optional fields on descriptors in
  IAMF v2). iamf-rs reads trim fields only on audio frames and does not parse
  those optional fields (test_000851).
- Audio element type 2 is object-based audio (IAMF v2), and types 3 to 7 are
  reserved. iamf-rs parses these by their declared size and does not decode
  them (test_000120). 73 of the vectors contain object-based elements.
- A malformed descriptor OBU other than the sequence header is skipped, as in
  iamf-tools. A bad sequence header still fails (test_000129).

All four example vectors are in the default `fetch_vectors.sh` set, so CI
parses them. `tools/iamfdec/tests/stream.rs` also decodes test_000116,
test_000120, and test_000129 end to end and checks that they produce output.
test_000851 is a Base-Advanced profile stream, which iamf-rs does not support.

## Limits

- One machine, shared with other work, with no CPU pinning. Ratios moved by up
  to 17% between runs (test_000002: 4.7 to 5.5).
- Not Chromium's build. Chromium uses its own clang, optimization flags,
  libc++ with hardening, PartitionAlloc, and its own libopus build. Absolute
  times and allocation sizes will differ there.
- Allocation counts cover the calling thread only.
- 10 streams, all decoded to stereo, 5.1, or 7.1.4. No binaural output, and no
  FLAC or AAC (both disabled in Chromium's iamf-tools build).
- Parity is against iamf-tools only. Agreement with iamf-tools does not show
  that either decoder matches the reference renders.

## Reproduce

```bash
LIBIAMF_TESTS_DIR=/path/to/libiamf/tests tools/fetch_vectors.sh --all
tools/fetch_vectors.sh --demo
CHROMIUM_SRC=/path/to/chromium/src python3 tools/bench_iamf_tools.py --strip-logs
```

Omit `--strip-logs` to build iamf-tools with logging compiled in.
`CHROMIUM_SRC` must contain `third_party/iamf_tools`, `third_party/abseil-cpp`,
and `third_party/opus`. Both decoders link the system libopus (`libopus-dev`).
Without `LIBIAMF_TESTS_DIR`, `--all` downloads from GitHub and gets only the
loudspeaker renders for mix presentation 42.
