#!/usr/bin/env bash
# F-238: crates.io へ公開する（--dry-run なら公開せずに、パッケージ化と検証のビルドだけを行う）。
#
#   tools/release/publish-crates.sh [--dry-run]
#
# 公開するのは veil-proxy-quiche・veil-proxy-wasmtime（vendoring）・veil-proxy-macros・veil-proxy・veil-proxy-build。
# vendoring の 2 つはワークスペースのメンバーではない（veil の clippy・test の対象にしないため）ので、
# メンバーに入れた作業用のコピーを作り、`cargo publish --workspace` で依存の順に公開する（cargo は前の
# クレートが索引に載るのを待ってから次を公開する）。既に同じ版が crates.io にあるクレートは飛ばすので、
# 途中で失敗しても再実行できる。
set -euo pipefail
DRY=0
[ "${1:-}" = --dry-run ] && DRY=1
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
STAGE="${STAGE_DIR:-$(mktemp -d)}/veil-publish"
rm -rf "$STAGE"
mkdir -p "$(dirname "$STAGE")"

# 作業用のコピー（追跡しているファイルだけ。未コミットの変更は含めない）
git -C "$ROOT" worktree add -q --detach "$STAGE" HEAD
trap 'git -C "$ROOT" worktree remove --force "$STAGE" >/dev/null 2>&1 || true' EXIT
cd "$STAGE"
python3 - <<'PY'
import re
p = "Cargo.toml"
s = open(p).read()
s = re.sub(r'^members = \[[^\]]*\]',
           'members = [".", "crates/veil-macros", "crates/veil-build", "third_party/quiche", "third_party/wasmtime"]',
           s, count=1, flags=re.M)
s = s.replace('    "third_party/wasmtime",\n', '').replace('    "third_party/quiche",\n', '')
open(p, "w").write(s)
PY

crates=(veil-proxy-quiche veil-proxy-wasmtime veil-proxy-macros veil-proxy veil-proxy-build)
exclude=()
for c in "${crates[@]}"; do
    ver="$(cargo metadata -q --no-deps --format-version 1 | python3 -c "import sys,json;print([p['version'] for p in json.load(sys.stdin)['packages'] if p['name']=='$c'][0])")"
    code="$(curl -s -o /dev/null -w '%{http_code}' -A 'veil-proxy release (https://github.com/aofusa/veil-proxy)' \
        "https://crates.io/api/v1/crates/$c/$ver")"
    if [ "$code" = 200 ]; then
        echo "skip $c $ver (already on crates.io)"
        exclude+=(--exclude "$c")
    else
        echo "publish $c $ver"
    fi
done

if [ "${#exclude[@]}" -eq $((${#crates[@]} * 2)) ]; then
    # 全部が公開済み（最初の版を手元から公開した後の rc など）。cargo package は対象が 0 個だと失敗するので何もしない
    echo "nothing to publish (all crates are already on crates.io)"
    exit 0
fi

if [ "$DRY" = 1 ]; then
    # 公開前のクレートに依存するクレートも、cargo package --workspace はまとめて検証できる
    cargo package --workspace "${exclude[@]}" --allow-dirty
else
    cargo publish --workspace "${exclude[@]}" --allow-dirty
fi
