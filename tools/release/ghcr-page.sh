#!/usr/bin/env bash
# F-246: ghcr.io のパッケージのページに説明（packaging/container/description.txt）を出す。
#
#   tools/release/ghcr-page.sh <image> <tag>      例: tools/release/ghcr-page.sh ghcr.io/aofusa/veil v0.8.3
#
# ghcr.io のページは、最後に push された版（version）の index の注釈 org.opencontainers.image.description を
# 名前の下に出す。公開の最後に push されるのは cosign の署名（`sha256-<digest>` のタグの index）なので、その index に
# 同じ注釈を付けて push し直す。cosign の検証は署名の manifest を見るので、index の注釈を足しても検証は変わらない
# （手元の registry と cosign v3 で確かめた）。
set -euo pipefail
IMAGE="${1:?image}"; TAG="${2:?tag}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DESC="$(cat "$ROOT/packaging/container/description.txt")"
[ "${#DESC}" -le 512 ] || { echo "error: packaging/container/description.txt is over 512 characters" >&2; exit 1; }
ann=(
    --annotation "index:org.opencontainers.image.description=$DESC"
    --annotation "index:org.opencontainers.image.source=https://github.com/aofusa/veil-proxy"
    --annotation "index:org.opencontainers.image.licenses=Apache-2.0 OR MIT"
)
# musl を後にする（ページの「Install」に出るのは最後に push した版。latest は musl）
for variant in glibc musl; do
    digest="$(docker buildx imagetools inspect "$IMAGE:$variant-$TAG" --format '{{json .Manifest}}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["digest"])')"
    sig="$IMAGE:sha256-${digest#sha256:}"
    if docker buildx imagetools inspect "$sig" >/dev/null 2>&1; then
        docker buildx imagetools create "${ann[@]}" -t "$sig" "$sig"
        echo "annotated $sig"
    else
        # 署名が無い（cosign の無い環境の公開）なら、版の index そのものが最後なので何もしない
        echo "no signature index for $IMAGE:$variant-$TAG"
    fi
done
