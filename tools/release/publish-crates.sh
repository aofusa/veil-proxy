#!/usr/bin/env bash
# F-238: crates.io へ公開する（--dry-run なら公開せずに、パッケージ化と検証のビルドだけを行う）。
#
#   tools/release/publish-crates.sh [--dry-run]
#
# --dry-run は作業用のコピーの版を次のマイナー版の -dryrun にしてから検証する（公開済みの版の依存で検証しないため）。
#
# 公開するのは veil-proxy-quiche・veil-proxy-wasmtime（vendoring）・veil-proxy-macros・veil-proxy・veil-proxy-build・
# veil-proxy-kit（F-267。veil-proxy に依存するので veil-proxy の後）。
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
           'members = [".", "crates/veil-macros", "crates/veil-build", "crates/veil-kit", "third_party/quiche", "third_party/wasmtime"]',
           s, count=1, flags=re.M)
s = s.replace('    "third_party/wasmtime",\n', '').replace('    "third_party/quiche",\n', '')
open(p, "w").write(s)
PY

if [ "$DRY" = 1 ] && [ "${DRY_RUN_VERSION:-}" != keep ]; then
    # 公開済みの版のままだと、変更したクレート（例: veil-proxy-kit）を crates.io の古い veil-proxy で検証してしまう。
    # 作業用のコピーだけ公開されていない版（次のマイナー版の -dryrun）にして、全部を一緒にパッケージ化・検証する
    # （DRY_RUN_VERSION=keep で今の版のまま）。0.9.2-dryrun では ^0.9.2-dryrun に公開済みの 0.9.2 が当たるので
    # マイナー版を上げる。vendoring の 2 つは版が別なので公開済みなら飛ばす。
    cur="$(awk '/^\[workspace.package\]/{w=1;next} /^\[/{w=0} w && /^version *=/{gsub(/[" ]/,"",$0); sub(/version=/,"",$0); print; exit}' Cargo.toml)"
    IFS=. read -r major minor _ <<<"${cur%%-*}"
    tools/release/bump-version.sh "${major}.$((minor + 1)).0-dryrun" >/dev/null
fi

crates=(veil-proxy-quiche veil-proxy-wasmtime veil-proxy-macros veil-proxy veil-proxy-build veil-proxy-kit)
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
