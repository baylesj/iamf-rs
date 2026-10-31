#!/usr/bin/env bash
# Fetches IAMF test vectors from the libiamf repository into tests/vectors/
# (git-ignored). Each vector is a standalone .iamf bitstream plus a
# .textproto describing it (is_valid_to_decode, codec, element types).
#
# Usage:
#   tools/fetch_vectors.sh              # curated v1.1 and v2.0 sets
#   tools/fetch_vectors.sh test_000123  # specific vector(s)
#   tools/fetch_vectors.sh --all        # every vector (~333, slow)
#   tools/fetch_vectors.sh --v2         # only the curated IAMF v2.0 set
#   tools/fetch_vectors.sh --demo      # real program material from the
#                                      # iamf-tools web demo (for iamfplay)
set -euo pipefail

# Pinned upstream revisions, so that CI results only change when these do
# (the CI vector cache is keyed on this file). Bump deliberately.
LIBIAMF_REV="63cafdd9b2d7bdba08cfeec2f008718198d7362e"
IAMF_TOOLS_REV="892f29f6de5e4e37d483e768bf00bcf580495d68"

BASE_URL="https://raw.githubusercontent.com/AOMediaCodec/libiamf/$LIBIAMF_REV/tests"
API_URL="https://api.github.com/repos/AOMediaCodec/libiamf/contents/tests"
DEST="$(cd "$(dirname "$0")/.." && pwd)/tests/vectors"

# Curated spread: LPCM/Opus/AAC-LC codecs, channel- and scene-based
# elements (ambisonics MONO and PROJECTION: 000042/000048), a redundant
# sequence header ahead of the canonical one (000079), streams with OBUs a
# decoder must skip (000116, 000120, 000129), plus two
# should-fail-to-decode cases (000007, 000025).
DEFAULT_VECTORS=(
  test_000002 test_000005 test_000007 test_000024 test_000025 test_000026
  test_000032 test_000033 test_000036 test_000038 test_000039 test_000042
  test_000048 test_000065 test_000066 test_000069 test_000070 test_000082
  test_000086 test_000088 test_000073 test_000090 test_000092 test_000079
  test_000611 test_000828 test_000829 test_000830
  test_000116 test_000120 test_000129
)

# IAMF v2.0 (Base-Advanced / Advanced-1 profile) vectors, all LPCM unless
# noted, picked small (~25 MB of .iamf): objects with every position
# parameter type (000800-000808, 000812), the 10.2.9.3 and 7.1.5.4
# expanded-layout families (000828 LFE pair, 000829/000830 bottom 3/4ch,
# 000831 top 1ch), two codec configs (000845), Metadata OBUs (000849),
# parameter blocks spanning frames (000850), mix presentation optional
# fields (000851/000852), element gain offsets (000853/000854), live
# loudness (000855), Advanced-1 objects (000912), and binaural output
# (001100-001103). The full v2 set is ~570 MB; name vectors to get more.
V2_VECTORS=(
  test_000800 test_000801 test_000802 test_000806 test_000807 test_000808
  test_000812 test_000827 test_000828 test_000829 test_000830 test_000831
  test_000845 test_000849 test_000850 test_000851 test_000852 test_000853
  test_000854 test_000855 test_000912 test_001100 test_001101 test_001102
  test_001103
)

# Optional local seeds, copied instead of downloaded when set:
#   LIBIAMF_TESTS_DIR  a libiamf checkout's tests/ directory (copies every
#                      rendered WAV for each vector, including binaural)
#   IAMF_DEMO_DIR      an iamf-tools checkout's gh_pages/web_demo/data/
LOCAL_TESTS_DIR="${LIBIAMF_TESTS_DIR:-}"
LOCAL_DEMO_DIR="${IAMF_DEMO_DIR:-}"

if [[ "${1:-}" == "--demo" ]]; then
  # Produced demo content shipped with the iamf-tools web demo (the
  # gh_pages/ directory at the pinned revision): 7.1.4 music in three codecs
  # plus a ~100 s third-order ambisonics soundtrack. Input for iamfplay.
  DEMO_URL="https://raw.githubusercontent.com/AOMediaCodec/iamf-tools/$IAMF_TOOLS_REV/gh_pages/web_demo/data"
  DEMO_DEST="$(cd "$(dirname "$0")/.." && pwd)/tests/demo"
  mkdir -p "$DEMO_DEST"
  for name in 7_1_4_Opus 7_1_4_Flac 7_1_4_PCM16_48000 \
    Animated_demo_3OA Animated_demo_3OA_and_2_0; do
    out="$DEMO_DEST/$name.iamf"
    [[ -s "$out" ]] && continue
    if [[ -n "$LOCAL_DEMO_DIR" && -s "$LOCAL_DEMO_DIR/$name.iamf" ]]; then
      cp --reflink=auto "$LOCAL_DEMO_DIR/$name.iamf" "$out" 2>/dev/null || cp "$LOCAL_DEMO_DIR/$name.iamf" "$out"
      echo "copied $name.iamf from $LOCAL_DEMO_DIR"
    elif curl -fsSL "$DEMO_URL/$name.iamf" -o "$out"; then
      echo "fetched $name.iamf"
    else
      echo "MISSING $name.iamf" >&2
      rm -f "$out"
    fi
  done
  echo "demo files in $DEMO_DEST"
  exit 0
fi

if [[ "${1:-}" == "--all" ]]; then
  echo "Listing all vectors..."
  if [[ -n "$LOCAL_TESTS_DIR" && -d "$LOCAL_TESTS_DIR" ]]; then
    echo "Found local tests dir: $LOCAL_TESTS_DIR"
    vectors=$(ls "$LOCAL_TESTS_DIR" | grep -oE '^test_[0-9_a-z]+\.iamf$' | sed -E 's/\.iamf$//' | sort -u)
  else
    vectors=$(curl -fsSL "$API_URL?per_page=100" --get --data-urlencode "ref=$LIBIAMF_REV" |
      grep -oE '"name": *"test_[0-9_a-z]+\.iamf"' | sed -E 's/.*"(test_[^"]+)\.iamf"/\1/')
    # The contents API paginates implicitly by directory listing; fall back to
    # the git tree API which returns everything in one call.
    if [[ $(echo "$vectors" | wc -l) -lt 100 ]]; then
      vectors=$(curl -fsSL "https://api.github.com/repos/AOMediaCodec/libiamf/git/trees/$LIBIAMF_REV?recursive=1" |
        grep -oE '"path": *"tests/test_[0-9_a-z]+\.iamf"' | sed -E 's|.*"tests/(test_[^"]+)\.iamf"|\1|')
    fi
  fi
  set -- $vectors
elif [[ "${1:-}" == "--v2" ]]; then
  set -- "${V2_VECTORS[@]}"
elif [[ $# -eq 0 ]]; then
  set -- "${DEFAULT_VECTORS[@]}" "${V2_VECTORS[@]}"
fi

mkdir -p "$DEST"
ok=0 fail=0
for name in "$@"; do
  if [[ -n "$LOCAL_TESTS_DIR" && -s "$LOCAL_TESTS_DIR/$name.iamf" ]]; then
    # Copy all rendered WAVs, .iamf, and .textproto for this vector from local seed.
    for f in "$LOCAL_TESTS_DIR/${name}_rendered_"*.wav; do
      [[ -e "$f" ]] || continue
      base="$(basename "$f")"
      [[ -s "$DEST/$base" ]] && continue
      cp --reflink=auto "$f" "$DEST/$base" 2>/dev/null || cp "$f" "$DEST/$base"
    done
    for ext in iamf textproto; do
      src="$LOCAL_TESTS_DIR/$name.$ext"
      out="$DEST/$name.$ext"
      if [[ -s "$src" && ! -s "$out" ]]; then
        cp --reflink=auto "$src" "$out" 2>/dev/null || cp "$src" "$out"
      fi
    done
    if [[ -s "$DEST/$name.iamf" ]]; then
      ok=$((ok + 1))
      continue
    fi
  fi

  # Remote fetch fallback:
  # Rendered reference WAVs (used by render conformance tests). All current
  # vectors use mix presentation id 42 / sub mix 0; missing layouts 404
  # harmlessly.
  for layout in 0 1 2 3 4 5 6 7 8 9; do
    ref="${name}_rendered_id_42_sub_mix_0_layout_${layout}.wav"
    [[ -s "$DEST/$ref" ]] && continue
    curl -fsSL "$BASE_URL/$ref" -o "$DEST/$ref" 2>/dev/null || rm -f "$DEST/$ref"
  done
  bin_ref="${name}_rendered_binaural.wav"
  if [[ ! -s "$DEST/$bin_ref" ]]; then
    curl -fsSL "$BASE_URL/$bin_ref" -o "$DEST/$bin_ref" 2>/dev/null || rm -f "$DEST/$bin_ref"
  fi
  for ext in iamf textproto; do
    out="$DEST/$name.$ext"
    if [[ -s "$out" ]]; then
      continue
    fi
    if curl -fsSL "$BASE_URL/$name.$ext" -o "$out"; then
      echo "fetched $name.$ext"
    else
      echo "MISSING $name.$ext" >&2
      rm -f "$out"
      [[ $ext == iamf ]] && fail=$((fail + 1))
      continue
    fi
  done
  [[ -s "$DEST/$name.iamf" ]] && ok=$((ok + 1))
done
echo "$ok vectors in $DEST${fail:+ ($fail missing)}"
