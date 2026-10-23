#!/usr/bin/env bash
# F-250: Homebrew の formula（veil.rb）を packaging/homebrew/veil.rb.in から作る。
#
#   tools/release/homebrew-formula.sh <version> <base-url> <SHA256SUMS> > veil.rb
#
# <base-url> は成果物の置き場所（本番は https://github.com/aofusa/veil-proxy/releases/download/v<ver>、rc の試験では
# runner の中の HTTP サーバ）。sha256 は F-247 の SHA256SUMS から引く。
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
VER="${1:?version}"; BASE="${2:?base-url}"; SUMS="${3:?SHA256SUMS}"
sha() {
    local v; v="$(awk -v f="$1" '$2==f || $2=="*"f {print $1}' "$SUMS")"
    [ -n "$v" ] || { echo "error: $1 is not in $SUMS" >&2; exit 1; }
    echo "$v"
}
M="$(sha "veil-$VER-universal2-apple-darwin.tar.gz")"
X="$(sha "veil-$VER-x86_64-unknown-linux-musl.tar.gz")"
A="$(sha "veil-$VER-aarch64-unknown-linux-musl.tar.gz")"
sed -e "s|@VERSION@|$VER|g" -e "s|@BASE@|$BASE|g" -e "s|@SHA_MACOS@|$M|" \
    -e "s|@SHA_LINUX_X86_64@|$X|" -e "s|@SHA_LINUX_AARCH64@|$A|" "$ROOT/packaging/homebrew/veil.rb.in"
