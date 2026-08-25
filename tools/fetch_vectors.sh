#!/usr/bin/env bash
# Fetches IAMF test vectors from the libiamf repository into tests/vectors/
# (git-ignored). Each vector is a standalone .iamf bitstream plus a
# .textproto describing it (is_valid_to_decode, codec, element types).
#
# Usage:
#   tools/fetch_vectors.sh              # curated default set
#   tools/fetch_vectors.sh test_000123  # specific vector(s)
#   tools/fetch_vectors.sh --all        # every vector (~333, slow)
#   tools/fetch_vectors.sh --demo      # real program material from the
#                                      # iamf-tools web demo (for iamfplay)
set -euo pipefail

BASE_URL="https://raw.githubusercontent.com/AOMediaCodec/libiamf/main/tests"
API_URL="https://api.github.com/repos/AOMediaCodec/libiamf/contents/tests"
DEST="$(cd "$(dirname "$0")/.." && pwd)/tests/vectors"

# Curated spread: LPCM/Opus/AAC-LC codecs, channel- and scene-based
# elements, plus two should-fail-to-decode cases (000007, 000025).
DEFAULT_VECTORS=(
  test_000002 test_000005 test_000007 test_000024 test_000025 test_000026
  test_000032 test_000033 test_000036 test_000038 test_000039 test_000065
  test_000066 test_000069 test_000070 test_000082 test_000086 test_000088
  test_000073 test_000090 test_000092
)

if [[ "${1:-}" == "--demo" ]]; then
  # Produced demo content shipped with the iamf-tools web demo (the
  # gh_pages/ directory of its main branch): 7.1.4 music in three codecs
  # plus a ~100 s third-order ambisonics soundtrack. Input for iamfplay.
  DEMO_URL="https://raw.githubusercontent.com/AOMediaCodec/iamf-tools/main/gh_pages/web_demo/data"
  DEMO_DEST="$(cd "$(dirname "$0")/.." && pwd)/tests/demo"
  mkdir -p "$DEMO_DEST"
  for name in 7_1_4_Opus 7_1_4_Flac 7_1_4_PCM16_48000 \
    Animated_demo_3OA Animated_demo_3OA_and_2_0; do
    out="$DEMO_DEST/$name.iamf"
    [[ -s "$out" ]] && continue
    if curl -fsSL "$DEMO_URL/$name.iamf" -o "$out"; then
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
  vectors=$(curl -fsSL "$API_URL?per_page=100" --get --data-urlencode "ref=main" |
    grep -oE '"name": *"test_[0-9_a-z]+\.iamf"' | sed -E 's/.*"(test_[^"]+)\.iamf"/\1/')
  # The contents API paginates implicitly by directory listing; fall back to
  # the git tree API which returns everything in one call.
  if [[ $(echo "$vectors" | wc -l) -lt 100 ]]; then
    vectors=$(curl -fsSL "https://api.github.com/repos/AOMediaCodec/libiamf/git/trees/main?recursive=1" |
      grep -oE '"path": *"tests/test_[0-9_a-z]+\.iamf"' | sed -E 's|.*"tests/(test_[^"]+)\.iamf"|\1|')
  fi
  set -- $vectors
elif [[ $# -eq 0 ]]; then
  set -- "${DEFAULT_VECTORS[@]}"
fi

mkdir -p "$DEST"
ok=0 fail=0
for name in "$@"; do
  # Rendered reference WAVs (used by render conformance tests). All current
  # vectors use mix presentation id 42 / sub mix 0; missing layouts 404
  # harmlessly.
  for layout in 0 1 2; do
    ref="${name}_rendered_id_42_sub_mix_0_layout_${layout}.wav"
    [[ -s "$DEST/$ref" ]] && continue
    curl -fsSL "$BASE_URL/$ref" -o "$DEST/$ref" 2>/dev/null || rm -f "$DEST/$ref"
  done
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
