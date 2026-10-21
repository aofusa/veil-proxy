#!/usr/bin/env bash
# F-237: アーキごとにダイジェストで push 済みのイメージを、マルチアーキのマニフェストにまとめてタグを付け、署名する。
#
#   tools/release/publish-images.sh <image> <tag> <prerelease:true|false> <digest-dir> [source-image]
#
# <digest-dir> には {musl,glibc}-{amd64,arm64} のファイル（中身は sha256:...）がある。source-image を
# 渡すと、そのリポジトリのダイジェストを <image> にコピーする（Docker Hub への複製）。
#
# タグ（例 v0.8.0）:
#   musl  → musl-v0.8.0、正式版なら musl・latest も
#   glibc → glibc-v0.8.0、正式版なら glibc も
# 版のタグが既にあれば上書きせず失敗する（動くタグだけが付け替わる）。
set -euo pipefail
IMAGE="${1:?image}"; TAG="${2:?tag}"; PRE="${3:?prerelease}"; DIR="${4:?digest dir}"; SRC="${5:-$1}"

for variant in musl glibc; do
    versioned="$IMAGE:$variant-$TAG"
    if docker buildx imagetools inspect "$versioned" >/dev/null 2>&1; then
        echo "error: $versioned already exists (version tags are never overwritten)" >&2
        exit 1
    fi
done

# F-246: ghcr.io のパッケージのページは、マルチアーキの index の注釈 org.opencontainers.image.description
# （テキストだけ、512 文字まで）を名前の下に出す（README は出ない）。
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DESC="$(cat "$ROOT/packaging/container/description.txt")"
[ "${#DESC}" -le 512 ] || { echo "error: packaging/container/description.txt is over 512 characters" >&2; exit 1; }
annotations=(
    --annotation "index:org.opencontainers.image.description=$DESC"
    --annotation "index:org.opencontainers.image.source=https://github.com/aofusa/veil-proxy"
    --annotation "index:org.opencontainers.image.licenses=Apache-2.0 OR MIT"
    --annotation "index:org.opencontainers.image.version=${TAG#v}"
)

for variant in musl glibc; do
    sources=()
    for arch in amd64 arm64; do
        f="$DIR/$variant-$arch"
        [ -s "$f" ] || { echo "error: missing digest $f" >&2; exit 1; }
        sources+=("$SRC@$(cat "$f")")
    done
    # 版のタグで index を作って署名してから、動くタグ（musl・glibc・latest）を同じ index に付ける。
    # ghcr.io のページは最後に push された版を「Install」に出すので、署名（sha256-<digest>.sig）を最後にしない。
    docker buildx imagetools create "${annotations[@]}" -t "$IMAGE:$variant-$TAG" "${sources[@]}"
    digest="$(docker buildx imagetools inspect "$IMAGE:$variant-$TAG" --format '{{json .Manifest}}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["digest"])')"
    echo "$IMAGE:$variant-$TAG -> $digest"
    if command -v cosign >/dev/null; then
        # キーレス署名（GitHub の OIDC。workflow の id-token: write）
        cosign sign --yes "$IMAGE@$digest"
    fi
    if [ "$PRE" != true ]; then
        tags=(-t "$IMAGE:$variant")
        [ "$variant" = musl ] && tags+=(-t "$IMAGE:latest")
        # 同じ index（ダイジェスト）にタグを付けるだけ（中身は版のタグと同じ）
        docker buildx imagetools create "${tags[@]}" "$IMAGE@$digest"
        for t in "${tags[@]}"; do
            [ "$t" = -t ] && continue
            got="$(docker buildx imagetools inspect "$t" --format '{{json .Manifest}}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["digest"])')"
            [ "$got" = "$digest" ] || { echo "error: $t -> $got (expected $digest)" >&2; exit 1; }
        done
    fi
done

# ページの説明: 最後に push された署名の index にも同じ注釈を付ける（tools/release/ghcr-page.sh）
if command -v cosign >/dev/null; then
    "$ROOT/tools/release/ghcr-page.sh" "$IMAGE" "$TAG"
fi
