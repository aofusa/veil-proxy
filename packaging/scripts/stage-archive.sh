#!/usr/bin/env bash
# F-247: リリースのアーカイブの中身を組み立てる（全 OS 共通）。
#
#   packaging/scripts/stage-archive.sh <binary> <target> <os> <out-dir>
#     <os>: linux | macos | windows | freebsd | openbsd | netbsd（例の設定の種類）
#
# <out-dir>/veil-<ver>-<target>/ に、バイナリ・LICENSE-*・README.md・examples/config.toml・examples/www/ を置く。
# 例の設定は自動では読まれない（バイナリだけの利用者は設定とページを自分で用意する。F-247）。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="${1:?binary}"; TARGET="${2:?target}"; OS="${3:?os}"; OUT="${4:?out-dir}"
VERSION="$(awk -F'"' '/^version = / { print $2; exit }' "$ROOT/Cargo.toml")"
NAME="veil-${VERSION}-${TARGET}"
DIR="$OUT/$NAME"

[ -f "$BIN" ] || { echo "error: binary not found: $BIN" >&2; exit 1; }
rm -rf "$DIR"; mkdir -p "$DIR"
case "$OS" in
    windows) install -m 0755 "$BIN" "$DIR/veil.exe" ;;
    *) install -m 0755 "$BIN" "$DIR/veil" ;;
esac
install -m 0644 "$ROOT/LICENSE-APACHE" "$ROOT/LICENSE-MIT" "$DIR/"
sed -e "s/@VERSION@/$VERSION/g" -e "s/@TARGET@/$TARGET/g" "$ROOT/packaging/archive/README.md" > "$DIR/README.md"
"$ROOT/tools/release/render-defaults.sh" "archive-$OS" "$DIR/examples"
echo "$DIR"
