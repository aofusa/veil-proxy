#!/usr/bin/env bash
# F-247: GitHub Release の draft を作る（あれば中身を差し替える）。公開は人が行う。
#
#   tools/release/github-draft.sh <tag> <title> <notes.md> <assets-dir> <prerelease:true|false>
#
# - 無ければ gh release create --draft（rc は --prerelease も）で作り、<assets-dir> の全ファイルを付ける
# - 同じタグの draft があれば（workflow の再実行）、本文を書き換え、成果物を --clobber で差し替え、無くなったものを消す
# - 公開済みのページがあれば何もしない（失敗にする。公開したものを CI が書き換えない）
set -euo pipefail
TAG="${1:?tag}"; TITLE="${2:?title}"; NOTES="${3:?notes}"; ASSETS="${4:?assets dir}"; PRE="${5:?prerelease}"
REPO="${GITHUB_REPOSITORY:-aofusa/veil-proxy}"

# B-208: API の一時的な失敗は none と見なさず失敗にする
state="$(GITHUB_REPOSITORY="$REPO" "$(dirname "$0")/release-state.sh" "$TAG")"
case "$state" in
    published)
        echo "::error title=GitHub Release::$TAG is already published; not touching it"
        exit 1 ;;
    draft)
        echo "updating the existing draft for $TAG"
        gh release edit "$TAG" --repo "$REPO" --title "$TITLE" --notes-file "$NOTES" --draft --prerelease="$PRE"
        gh release upload "$TAG" --repo "$REPO" --clobber "$ASSETS"/*
        want="$(cd "$ASSETS" && ls)"
        for a in $(gh release view "$TAG" --repo "$REPO" --json assets --jq '.assets[].name'); do
            grep -qxF "$a" <<<"$want" || gh release delete-asset "$TAG" "$a" --repo "$REPO" --yes
        done ;;
    none)
        args=(--repo "$REPO" --draft --verify-tag --title "$TITLE" --notes-file "$NOTES")
        [ "$PRE" = true ] && args+=(--prerelease)
        gh release create "$TAG" "${args[@]}" "$ASSETS"/* ;;
esac
n="$(gh release view "$TAG" --repo "$REPO" --json assets --jq '.assets | length')"
url="$(gh release view "$TAG" --repo "$REPO" --json url --jq .url)"
echo "::notice title=GitHub Release draft::$TAG has $n assets: $url"
