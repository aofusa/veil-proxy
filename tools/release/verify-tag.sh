#!/usr/bin/env bash
# F-231: 公開の前の検査。タグ（v を除く）とワークスペースの版が一致し、タグのコミットが main に
# 含まれていることを確かめ、GitHub Actions の出力（tag= version= prerelease=）を標準出力に書く。
#
#   tools/release/verify-tag.sh v0.8.0
set -euo pipefail
TAG="${1:?usage: $0 <tag>}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

if ! [[ "$TAG" =~ ^v([0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?)$ ]]; then
    echo "error: tag '$TAG' is not vX.Y.Z[-pre]" >&2
    exit 1
fi
TAG_VERSION="${BASH_REMATCH[1]}"
WS_VERSION="$(awk '/^\[workspace.package\]/{w=1;next} /^\[/{w=0} w && /^version *=/{gsub(/[" ]/,"");sub(/version=/,"");print;exit}' Cargo.toml)"
if [ "$TAG_VERSION" != "$WS_VERSION" ]; then
    echo "error: tag $TAG does not match the workspace version $WS_VERSION (run tools/release/bump-version.sh)" >&2
    exit 1
fi
# crates/* が版を継いでいること（版を直書きしたメンバーがあれば、公開物の版がずれる）
for m in crates/veil-macros crates/veil-build crates/veil-python crates/veil-capi; do
    if ! grep -q '^version.workspace = true' "$m/Cargo.toml"; then
        echo "error: $m/Cargo.toml does not use version.workspace = true" >&2
        exit 1
    fi
done
# タグのコミットが main にあること（ブランチの途中のコミットを公開しない）
git fetch -q origin main 2>/dev/null || true
if git rev-parse -q --verify origin/main >/dev/null; then
    if ! git merge-base --is-ancestor HEAD origin/main; then
        echo "error: the tagged commit is not on main" >&2
        exit 1
    fi
else
    echo "warning: origin/main not found; skipping the main-branch check" >&2
fi
pre=false
case "$TAG_VERSION" in *-*) pre=true ;; esac
echo "tag=$TAG"
echo "version=$TAG_VERSION"
echo "prerelease=$pre"
