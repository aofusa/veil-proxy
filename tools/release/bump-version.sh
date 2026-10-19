#!/usr/bin/env bash
# F-231: veil の版を上げる。
#
#   tools/release/bump-version.sh 0.8.0
#
# 版の出所はルートの Cargo.toml の [workspace.package] version だけ（ルートと crates/* はそれを継ぐ。
# Python の wheel の版は maturin が crates/veil-python の版を読む）。この台本はそこを書き換え、
# 版を書き写している次の場所も揃える。同じ版で 2 回実行しても差分は出ない。
#
#   - ルートから veil-macros への依存の version
#   - Cargo.lock（ワークスペース）と、ワークスペース外のクレートの Cargo.lock の veil 系の版
#   - 文書の中で「今の版の例」として書いた箇所（下の DOC_FILES。計測の基準として書いた版
#     ＝ docs/perf・docs/guide/performance.md の表・tools/perf は意味が変わるので書き換えない）
#
# 公開の workflow（.github/workflows/release.yml）は、タグ（v を除く）とこの版が違えば何も公開しない。
set -euo pipefail

NEW="${1:-}"
if ! [[ "$NEW" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
    echo "usage: $0 <X.Y.Z[-pre]>" >&2
    exit 2
fi
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
cd "$ROOT"

OLD="$(awk '/^\[workspace.package\]/{w=1;next} /^\[/{w=0} w && /^version *=/{gsub(/[" ]/,"",$0); sub(/version=/,"",$0); print; exit}' Cargo.toml)"
if [ -z "$OLD" ]; then
    echo "error: [workspace.package] version not found in Cargo.toml" >&2
    exit 1
fi
echo "version: $OLD -> $NEW"

# 1. ワークスペースの版と、veil-macros への依存の版
python3 - "$OLD" "$NEW" <<'PY'
import re, sys
old, new = sys.argv[1], sys.argv[2]
p = "Cargo.toml"
s = open(p).read()
head, sep, rest = s.partition("[workspace.package]\n")
if not sep:
    sys.exit("no [workspace.package]")
rest = re.sub(r'^version = "[^"]*"', f'version = "{new}"', rest, count=1, flags=re.M)
s = head + sep + rest
s = re.sub(r'(veil-(?:proxy-)?macros = \{[^}\n]*version = ")[^"]*(")', rf'\g<1>{new}\g<2>', s)
open(p, "w").write(s)
PY

# 2. Cargo.lock（ワークスペースと、path で veil に依存するワークスペース外のクレート）
#    path 依存の版だけを解決し直す（他の依存は動かさない）。
cargo update -w --offline -q 2>/dev/null || cargo update -w -q
for lock in examples/api/*/Cargo.lock tools/api-bench/*/Cargo.lock tools/tokio-compat/*/Cargo.lock \
    tests/*/Cargo.lock tests/api/*/Cargo.lock crates/veil-tokio/tokio-shim/Cargo.lock; do
    [ -f "$lock" ] || continue
    dir="$(dirname "$lock")"
    pkgs=()
    for name in veil veil-proxy veil-macros veil-proxy-macros veil-build veil-proxy-build; do
        if grep -q "^name = \"$name\"$" "$lock"; then pkgs+=(-p "$name"); fi
    done
    [ "${#pkgs[@]}" -gt 0 ] || continue
    (cd "$dir" && { cargo update "${pkgs[@]}" --offline -q 2>/dev/null || cargo update "${pkgs[@]}" -q; })
done

# 3. 文書の「今の版の例」
DOC_FILES=(
    docs/guide/building.md docs/guide/ja/building.md
    docs/guide/vds.md docs/guide/ja/vds.md
    packaging/README.md
    .github/workflows/release-sbom.yml
)
python3 - "$NEW" "${DOC_FILES[@]}" <<'PY'
import re, sys
new, files = sys.argv[1], sys.argv[2:]
V = r'[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?'
pats = [
    (rf'(veil_){V}(_amd64\.deb|_arm64\.deb)', rf'\g<1>{new}\g<2>'),
    (rf'(veil-){V}(-1\.(?:x86_64|aarch64)\.rpm)', rf'\g<1>{new}\g<2>'),
    (rf'(veil-){V}(-(?:x86_64|aarch64)-unknown-linux-(?:gnu|musl)\.tar\.gz)', rf'\g<1>{new}\g<2>'),
    (rf'("user_agent_version": "){V}(")', rf'\g<1>{new}\g<2>'),
    (rf'(リリースのタグ（例: v){V}(）)', rf'\g<1>{new}\g<2>'),
]
for f in files:
    s = open(f).read()
    t = s
    for a, b in pats:
        t = re.sub(a, b, t)
    if t != s:
        open(f, "w").write(t)
        print("updated", f)
PY

echo "done. check: cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | .name + \" \" + .version'"
