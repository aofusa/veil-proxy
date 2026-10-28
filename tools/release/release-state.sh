#!/usr/bin/env bash
# B-208: GitHub Release の状態を出す（published / draft / none）。release.yml（B-168 の判定）と github-draft.sh が使う。
#
#   tools/release/release-state.sh <tag>        # 標準出力に published・draft・none のどれか
#   tools/release/release-state.sh --self-test  # 偽の gh で判定を確かめる
#
# 「無い」と言えるのは gh が release not found と答えたときだけ。API の一時的な失敗（レート制限・5xx・認証）は
# none と見なさず失敗にする（公開済みのタグの再実行で、作り直し・署名・attestation を走らせない）。
set -euo pipefail

state() {
    local tag="$1" repo="${GITHUB_REPOSITORY:-aofusa/veil-proxy}" err out
    err="$(mktemp)"
    if out="$(gh release view "$tag" --repo "$repo" --json isDraft --jq .isDraft 2>"$err")"; then
        rm -f "$err"
        case "$out" in
            true) echo draft ;;
            false) echo published ;;
            *) echo "release-state: unexpected answer from gh: $out" >&2; return 1 ;;
        esac
        return 0
    fi
    if grep -qi 'release not found' "$err"; then
        rm -f "$err"
        echo none
        return 0
    fi
    echo "::error title=GitHub Release::could not read the state of $tag" >&2
    cat "$err" >&2
    rm -f "$err"
    return 1
}

self_test() {
    local d fails=0
    d="$(mktemp -d)"
    trap 'rm -rf "$d"' RETURN
    cat > "$d/gh" <<'GH'
#!/usr/bin/env bash
case "$FAKE_GH" in
    published) echo false ;;
    draft) echo true ;;
    missing) echo "release not found" >&2; exit 1 ;;
    error) echo "HTTP 502: Bad Gateway (https://api.github.com/...)" >&2; exit 1 ;;
esac
GH
    chmod +x "$d/gh"
    for c in published:published draft:draft missing:none; do
        got="$(PATH="$d:$PATH" FAKE_GH="${c%%:*}" state v0.0.0)" || got="(failed)"
        [ "$got" = "${c##*:}" ] || { echo "FAIL ${c%%:*}: $got"; fails=1; }
    done
    if PATH="$d:$PATH" FAKE_GH=error state v0.0.0 >/dev/null 2>&1; then
        echo "FAIL error: an API failure was not reported"; fails=1
    fi
    [ "$fails" = 0 ] && echo "release-state self-test ok"
    return "$fails"
}

case "${1:?usage: release-state.sh <tag> | --self-test}" in
    --self-test) self_test ;;
    *) state "$1" ;;
esac
