Reference WAVs for binaural (HRTF) conformance tests, generated with
iamf-tools' `decoder_main --output_layout Binaural` (which renders through
google/obr) from the corresponding libiamf test vectors with
`headphones_rendering_mode` set to 1 (BINAURAL). The tests recreate the
mode-1 bitstream variants by patching the fetched vectors in memory.
