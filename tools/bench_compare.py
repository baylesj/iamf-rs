#!/usr/bin/env python3
"""Wall-clock comparison: libiamf v1.1 iamfdec vs iamf-rs iamfdec on the
same vectors and output layouts. Both run their peak limiter (libiamf's is
always on). Reports median of N whole-process runs and a sample diff.

Usage: tools/bench_compare.py <path-to-libiamf-iamfdec>
Build libiamf v1.1: clone the v1.1.0 tag, run code/dep_codecs/build.sh
(CMAKE_POLICY_VERSION_MINIMUM=3.5 may be needed with CMake 4), cmake the
top level with -DCMAKE_INSTALL_PREFIX, install, then cmake
code/test/tools/iamfdec."""
import subprocess, time, statistics, sys, os, wave

import pathlib
ROOT = pathlib.Path(__file__).resolve().parent.parent
RS = str(ROOT / "target/release/iamfdec")
C = sys.argv[1]  # path to libiamf iamfdec binary
VEC = str(ROOT / "tests/vectors")
import tempfile
OUT = tempfile.mkdtemp(prefix="iamfrs_bench_")
N = 12

CASES = [
    ("test_000070", 0, "lpcm 7.1.4 -> stereo", 5.0),
    ("test_000070", 9, "lpcm 7.1.4 -> 7.1.4", 5.0),
    ("test_000026", 0, "opus stereo -> stereo", 0.5),
    ("test_000038", 0, "lpcm FOA -> stereo", 0.5),
    ("test_000086", 2, "lpcm multi-element -> 5.1.2", 5.0),
]

def run_timed(cmd, cwd=None):
    times = []
    for _ in range(N):
        t0 = time.perf_counter()
        r = subprocess.run(cmd, cwd=cwd, stdout=subprocess.DEVNULL,
                           stderr=subprocess.DEVNULL)
        times.append(time.perf_counter() - t0)
        if r.returncode != 0:
            return None, r.returncode
    return statistics.median(times), 0

def read_wav(path):
    w = wave.open(path)
    return w.getnchannels(), w.readframes(w.getnframes())

print(f"{'case':32} {'libiamf':>10} {'iamf-rs':>10} {'ratio':>7}  output")
for name, ss, label, secs in CASES:
    src = f"{VEC}/{name}.iamf"
    c_out = f"{OUT}/c_{name}_{ss}.wav"
    rs_out = f"{OUT}/rs_{name}_{ss}.wav"

    t_c, rc = run_timed([C, f"-s{ss}", "-o3", c_out, src], cwd=OUT)
    if rc != 0:
        print(f"{label:32} libiamf failed rc={rc}")
        continue
    t_rs, rc = run_timed([RS, src, "-o", rs_out, "-s", str(ss), "--limiter"])
    if rc != 0:
        print(f"{label:32} iamf-rs failed rc={rc}")
        continue

    # Compare outputs.
    match = "?"
    try:
        ca, a = read_wav(c_out)
        cb, b = read_wav(rs_out)
        if ca != cb or len(a) != len(b):
            match = f"shape differs ({ca}ch/{len(a)}B vs {cb}ch/{len(b)}B)"
        else:
            ia = memoryview(a).cast('h')
            ib = memoryview(b).cast('h')
            maxd = max((abs(x - y) for x, y in zip(ia, ib)), default=0)
            exact = sum(1 for x, y in zip(ia, ib) if x == y)
            match = f"max diff {maxd} ({100*exact//max(len(ia),1)}% exact)"
    except Exception as e:
        match = f"compare failed: {e}"

    print(f"{label:32} {t_c*1000:8.1f}ms {t_rs*1000:8.1f}ms {t_c/t_rs:6.2f}x  {match}")
