#!/usr/bin/env python3
"""Extracts obr's embedded SH-HRIR binaural filter WAVs (byte arrays in
binaural_filters_*.cc) into binary .wav assets for iamf-dec.

The filters carry obr's BSD-3-Clause-Clear license and the Open Binaural
Renderer Patent License 1.0 (see the NOTICE file written alongside).

Usage: tools/extract_binaural_filters.py <path-to-obr-checkout>
Only the ambient (iamf-tools default) and direct profiles are extracted;
reverberant can be added the same way when needed."""
import re
import sys
import pathlib

OUT = pathlib.Path(__file__).resolve().parent.parent / "crates/iamf-dec/assets/binaural"

PROFILES = ["ambient", "direct"]
ORDERS = [1, 2, 3, 4]
EARS = ["l", "r"]

NOTICE = """These SH-HRIR binaural filter assets are extracted from the Open
Binaural Renderer (https://github.com/google/obr), Copyright (c) 2025
Google LLC, and are subject to the BSD 3-Clause Clear License (see the
obr repository's LICENSE file) and the Open Binaural Renderer Patent
License 1.0 (see its PATENTS file). Extracted mechanically by
tools/extract_binaural_filters.py; the WAV payloads are byte-identical
to the upstream arrays.
"""

def main():
    obr = pathlib.Path(sys.argv[1])
    src_dir = obr / "obr/ambisonic_binaural_decoder/binaural_filters"
    OUT.mkdir(parents=True, exist_ok=True)
    (OUT / "NOTICE").write_text(NOTICE)
    total = 0
    for profile in PROFILES:
        for order in ORDERS:
            for ear in EARS:
                src = src_dir / f"binaural_filters_{order}_oa_{profile}_{ear}.cc"
                text = src.read_text()
                data = bytes(
                    int(tok, 16) for tok in re.findall(r"0x[0-9a-fA-F]{2}", text)
                )
                assert data[:4] == b"RIFF" and data[8:12] == b"WAVE", src
                dst = OUT / f"{order}oa_{profile}_{ear}.wav"
                dst.write_bytes(data)
                total += len(data)
                print(f"{dst.name}: {len(data)} bytes")
    print(f"total {total} bytes -> {OUT}")

if __name__ == "__main__":
    main()
