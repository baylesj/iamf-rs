# iamf-rs vs. iamf-tools

Chromium plays IAMF with iamf-tools v3.0.0. We built iamf-tools and iamf-rs
into one test binary, set up the way Chromium sets up iamf-tools. Then we
decoded the same 10 streams with each.

In short:

- **Output:** iamf-rs produced the same audio, within 1 LSB (least
  significant bit) at 16 bits.
- **Speed:** iamf-rs was 4.6x to 7.8x faster on LPCM (uncompressed) streams,
  and 1.3x to 2.0x faster on Opus streams.
- **Allocations:** iamf-rs made a half to a third as many heap allocations
  per frame.
- **Not ahead everywhere:** on some Opus streams, iamf-rs allocates more
  bytes.

These results were measured with iamf-rs at commit 19520a8. Details are
below.

## Results

Times are the median of 3 runs. Speedup is the range across those runs. A
TU (temporal unit) is one frame of output.

| Stream | Output | C++ ms | Rust ms | Speedup | Allocs per TU, C++ / Rust | Max diff, 16-bit LSB |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| 000002 LPCM stereo | stereo | 1.55 | 0.28 | 4.7-5.5 | 41 / 21 | 0 |
| 000026 Opus stereo | stereo | 2.91 | 1.47 | 1.9-2.0 | 42 / 21 | 0 |
| 000036 LPCM scalable | 5.1 | 98.8 | 18.0 | 5.5 | 115 / 47 | 1 |
| 000038 LPCM ambisonics | stereo | 8.50 | 1.33 | 6.3-6.4 | 61 / 28 | 0 |
| 000070 LPCM 7.1.4 | 7.1.4 | 166 | 36.4 | 4.6 | 148 / 75 | 0 |
| 000086 LPCM ambisonics + scalable | stereo | 200 | 25.7 | 7.7-7.8 | 178 / 59 | 1 |
| demo LPCM 7.1.4 | 7.1.4 | 833 | 143 | 5.8 | 148 / 75 | 0 |
| demo Opus 7.1.4 | 7.1.4 | 342 | 266 | 1.3 | 148 / 75 | 1 |
| demo Opus 3OA | stereo | 2593 | 2023 | 1.3 | 188 / 66 | 1 |
| demo Opus 3OA + stereo | stereo | 2790 | 2166 | 1.3 | 217 / 79 | 1 |

The numbered streams are libiamf test vectors (0.5 s to 5 s). The demos are
the iamf-tools web demo files (25 s and 99 s).

Decoder creation:

| Stream | Time, C++ / Rust (us) | Allocs, C++ / Rust | KiB, C++ / Rust |
| --- | ---: | ---: | ---: |
| 000002 LPCM stereo | 21 / 3 | 166 / 30 | 16 / 5 |
| 000026 Opus stereo | 135 / 109 | 168 / 32 | 245 / 246 |
| 000036 LPCM scalable | 70 / 15 | 256 / 36 | 50 / 6 |
| 000038 LPCM ambisonics | 29 / 5 | 220 / 33 | 18 / 5 |
| 000070 LPCM 7.1.4 | 81 / 17 | 312 / 41 | 133 / 6 |
| 000086 LPCM ambisonics + scalable | 106 / 22 | 434 / 56 | 34 / 7 |
| demo LPCM 7.1.4 | 82 / 13 | 315 / 41 | 133 / 6 |
| demo Opus 7.1.4 | 935 / 808 | 328 / 55 | 1523 / 1629 |
| demo Opus 3OA | 1736 / 1664 | 399 / 74 | 3116 / 3361 |
| demo Opus 3OA + stereo | 1913 / 1822 | 529 / 95 | 3334 / 3602 |

## What the numbers say

**Output.** Channel count, sample rate, and length matched on every stream.
Chromium uses 32-bit output, and three streams are identical at 32 bits. The
largest 32-bit difference is on 000086: about 0.66 of a 16-bit LSB, or 94 dB
below full scale. This shows agreement with iamf-tools, not with the libiamf
reference renders.

**Speed.** The gap is smaller on Opus because both decoders call the same
libopus, which likely takes much of the time. We have not profiled either
decoder, so we don't know yet where the LPCM gap comes from. iamf-rs also had
lower median (p50) and p99 per-frame latency on every stream.

**Memory.** iamf-rs makes 5x to 8x fewer allocations at creation, and 2x to
3x fewer per frame. By bytes, it is not always smaller:

- At creation, it allocates 7% to 8% more on the multichannel Opus streams.
- Per frame, it allocates more on Opus stereo (60 vs. 37 KiB) and Opus 7.1.4
  (325 vs. 188 KiB).

These are worth a look. Peak heap growth during decode was 2x to 3.3x lower,
but that figure leaves out memory allocated at creation.

**Logging.** iamf-tools logs at INFO level while it creates a decoder. Each
log call allocates about 26 KiB, even when nothing is printed. That added 1.3
to 1.9 MiB per decoder. We found nothing in Chromium's build that compiles
these logs out, so Chrome may pay this cost. We haven't checked this in a
real Chrome build. The numbers above use an iamf-tools build with logging
compiled out (`ABSL_MIN_LOG_LEVEL=99`), which favors iamf-tools.

## How we measured

`tools/bench_iamf_tools.cc` links both decoders and calls them through the
same interface Chromium's `IamfAudioDecoder` uses:

- iamf-tools is built from the source list in Chromium's
  `third_party/iamf_tools/BUILD.gn`, with the same defines and the
  `strip_errors.h` include from Chromium's release build.
- iamf-rs is `iamf-capi` built with Chromium's features (`pcm`, `opus-ffi`).
- Both get Chromium's settings (32-bit output, `kOrderingForAndroid`) and link
  the same system libopus 1.6.1.

For each stream, the harness does three things:

1. It decodes once with each decoder and compares every sample.
2. It counts heap allocations by wrapping glibc's `malloc` family.
3. It times creation and decode, with counting turned off.

The two decoders take turns running first. Allocation counts were the same
on every run.

The machine was a shared AMD EPYC 7B13 with clang 21.1.8 (`-O3`, libstdc++)
and rustc 1.93.1 (default release profile), no LTO on either side.

The tables were measured with iamf-rs at main 19520a8. We reran on top of
231a56d, which added expanded layouts, v2 parsing, and §7.4.1 mix selection.
The rerun gave the same output and the same per-frame allocations. iamf-rs
made one more allocation at creation. The machine was busier during the
rerun, so both decoders were slower. The speedups stayed in a similar range:
LPCM 4.6x to 9.8x, Opus 1.2x to 2.0x.

## Caveats

- One shared machine with no CPU pinning. Speedups moved by up to 17% between
  runs.
- This is not Chromium's build. Chromium uses its own clang, flags, libc++,
  PartitionAlloc, and libopus, so absolute numbers will differ there.
- Allocations are counted on the calling thread only.
- Only stereo, 5.1, and 7.1.4 output. No binaural, FLAC, or AAC (Chromium
  disables the last three in iamf-tools).

## Related changes

For this work, we ran all 333 libiamf vectors through the parser. We found
that iamf-rs rejected streams with a malformed descriptor that iamf-tools
skips. The same change fixed that. Checking decoded output against every
reference render is a separate effort.

## Running it

```bash
LIBIAMF_TESTS_DIR=/path/to/libiamf/tests tools/fetch_vectors.sh --all
tools/fetch_vectors.sh --demo
CHROMIUM_SRC=/path/to/chromium/src python3 tools/bench_iamf_tools.py --strip-logs
```

- `CHROMIUM_SRC` must contain `third_party/iamf_tools`,
  `third_party/abseil-cpp`, and `third_party/opus`.
- Drop `--strip-logs` to keep the iamf-tools logging.
- Without `LIBIAMF_TESTS_DIR`, `--all` downloads from GitHub and gets fewer
  reference renders.
