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

for variant in musl glibc; do
    sources=()
    for arch in amd64 arm64; do
        f="$DIR/$variant-$arch"
        [ -s "$f" ] || { echo "error: missing digest $f" >&2; exit 1; }
        sources+=("$SRC@$(cat "$f")")
    done
    tags=(-t "$IMAGE:$variant-$TAG")
    if [ "$PRE" != true ]; then
        tags+=(-t "$IMAGE:$variant")
        [ "$variant" = musl ] && tags+=(-t "$IMAGE:latest")
    fi
    docker buildx imagetools create "${tags[@]}" "${sources[@]}"
    digest="$(docker buildx imagetools inspect "$IMAGE:$variant-$TAG" --format '{{json .Manifest}}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["digest"])')"
    echo "$IMAGE:$variant-$TAG -> $digest"
    if command -v cosign >/dev/null; then
        # キーレス署名（GitHub の OIDC。workflow の id-token: write）
        cosign sign --yes "$IMAGE@$digest"
    fi
done
