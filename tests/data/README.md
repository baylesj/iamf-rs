# Binaural reference renders

These WAV files are reference outputs for the binaural (HRTF) conformance
tests in `tools/iamfdec/tests/binaural.rs`.

| File | Source vector |
| --- | --- |
| `test_000038_mode1_binaural_ref.wav` | libiamf `test_000038` |
| `test_000070_mode1_binaural_ref.wav` | libiamf `test_000070` |

How they were made:

1. Take the libiamf test vector and set `headphones_rendering_mode` to 1
   (binaural).
2. Decode it with the iamf-tools `decoder_main --output_layout Binaural`.
   iamf-tools renders binaural output through google/obr.

The tests don't store the patched bitstreams. They fetch the vectors and
apply the same patch in memory.

The IAMF v2.0 binaural tests don't use this directory. They compare with
the `*_rendered_binaural.wav` files that `tools/fetch_vectors.sh` downloads.
