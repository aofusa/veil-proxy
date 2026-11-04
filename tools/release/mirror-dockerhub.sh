#!/usr/bin/env bash
# F-248: 公開した版のイメージを ghcr.io から Docker Hub へ写す（release-downstream.yml。リリースのページの公開のとき）。
#
#   tools/release/mirror-dockerhub.sh <tag> <prerelease:true|false>
#
# - oras copy -r で、版のタグの index と、それを指す署名・SBOM（OCI の referrer）をそのまま写す（digest は ghcr.io と同じ）。
#   cosign v3 の署名は referrer なので、cosign copy（非推奨）では Docker Hub の cosign verify が署名を見つけられない
# - 版だけのタグ（vX.Y.Z）を musl と同じ digest に付ける（プレリリースも）。正式版なら latest・musl・glibc も付ける
# - 写した後、digest が ghcr.io と同じで cosign verify が通ることを確かめる
# - Docker Hub の説明（短い説明と packaging/container/dockerhub.md）を API で更新する
# 要る環境変数: DOCKERHUB_USERNAME・DOCKERHUB_TOKEN（docker login 済みであること）
set -euo pipefail
TAG="${1:?tag}"; PRE="${2:?prerelease}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SRC=ghcr.io/aofusa/veil
DST="docker.io/${DOCKERHUB_NAMESPACE:-aofusa}/veil"
digest_of() { docker buildx imagetools inspect "$1" --format '{{json .Manifest}}' | python3 -c 'import sys,json;print(json.load(sys.stdin)["digest"])'; }

for variant in musl glibc; do
    src="$SRC:$variant-$TAG"
    d="$(digest_of "$src")"
    oras copy -r "$src" "$DST:$variant-$TAG"
    got="$(digest_of "$DST:$variant-$TAG")"
    [ "$got" = "$d" ] || { echo "::error title=Docker Hub::$DST:$variant-$TAG is $got, ghcr.io has $d"; exit 1; }
    tags=()
    [ "$variant" = musl ] && tags+=(-t "$DST:$TAG")
    if [ "$PRE" != true ]; then
        tags+=(-t "$DST:$variant"); [ "$variant" = musl ] && tags+=(-t "$DST:latest")
    fi
    if [ "${#tags[@]}" -gt 0 ]; then
        docker buildx imagetools create "${tags[@]}" "$DST@$d"
        for t in "${tags[@]}"; do
            [ "$t" = -t ] && continue
            got="$(digest_of "$t")"
            [ "$got" = "$d" ] || { echo "::error title=Docker Hub::$t is $got, expected $d"; exit 1; }
        done
    fi
    # B-290: Docker Hub は写した署名（referrer）を少し遅れて返すことがある（直後の verify が no signatures found）。
    # 間を置いて数回だけ確かめ直す
    for i in 1 2 3 4 5 6; do
        cosign verify "$DST@$d" \
            --certificate-identity-regexp '^https://github.com/aofusa/veil-proxy/' \
            --certificate-oidc-issuer https://token.actions.githubusercontent.com >/dev/null && break
        [ "$i" = 6 ] && { echo "::error title=Docker Hub::no valid signature for $DST@$d"; exit 1; }
        echo "signature of $DST@$d not visible yet, retrying in 20s"
        sleep 20
    done
    echo "ok   $DST:$variant-$TAG = $d (signature verified)"
done

# 説明（短い説明は 100 文字まで）
short="High-performance reverse proxy and web server (HTTP/1.1, 2, 3, gRPC, Proxy-Wasm). Mirror of ghcr.io."
[ "${#short}" -le 100 ] || { echo "error: the short description is over 100 characters" >&2; exit 1; }
jwt="$(curl -fsS -H 'Content-Type: application/json' \
    -d "$(python3 -c 'import json,os;print(json.dumps({"username":os.environ["DOCKERHUB_USERNAME"],"password":os.environ["DOCKERHUB_TOKEN"]}))')" \
    https://hub.docker.com/v2/users/login/ | python3 -c 'import sys,json;print(json.load(sys.stdin)["token"])')"
python3 - "$ROOT/packaging/container/dockerhub.md" "$short" > "$RUNNER_TEMP/desc.json" <<'PY'
import json, sys
print(json.dumps({"full_description": open(sys.argv[1], encoding="utf-8").read(), "description": sys.argv[2]}))
PY
curl -fsS -X PATCH -H "Authorization: JWT $jwt" -H 'Content-Type: application/json' \
    --data @"$RUNNER_TEMP/desc.json" "https://hub.docker.com/v2/repositories/${DOCKERHUB_NAMESPACE:-aofusa}/veil/" >/dev/null
echo "ok   Docker Hub description updated"
