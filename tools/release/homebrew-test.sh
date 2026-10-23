#!/usr/bin/env bash
# F-250: 手元の tap に formula を置いて、brew install・brew test・既定の設定での起動・入れ直し・消すを試験する（CI の runner）。
#
#   tools/release/homebrew-test.sh <dist-dir>      # dist-dir に macOS・Linux musl のアーカイブがある
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
DIST="$(cd "${1:?dist dir}" && pwd)"
FAILS=0
ok() { echo "ok   $*"; }
bad() { echo "FAIL $*"; echo "::error title=homebrew ($(uname -sm))::$*"; FAILS=$((FAILS+1)); }
step() { local label="$1"; shift; local out; if out="$("$@" 2>&1)"; then ok "$label"; else bad "$label: $(tail -5 <<<"$out" | tr '\n' ' ' | cut -c1-700)"; echo "$out" | tail -40; return 1; fi; }

if ! command -v brew >/dev/null; then eval "$(/home/linuxbrew/.linuxbrew/bin/brew shellenv 2>/dev/null || true)"; fi
if ! command -v brew >/dev/null; then
    NONINTERACTIVE=1 /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)" >/dev/null
    eval "$(/home/linuxbrew/.linuxbrew/bin/brew shellenv)"
fi
export HOMEBREW_NO_AUTO_UPDATE=1 HOMEBREW_NO_ANALYTICS=1 HOMEBREW_NO_INSTALL_CLEANUP=1
v="$(awk -F'"' '/^version = / { print $2; exit }' "$ROOT/Cargo.toml")"
(cd "$DIST" && shasum -a 256 ./*.tar.gz | sed 's| \./| |' > SHA256SUMS)
(cd "$DIST" && nohup python3 -m http.server 18600 --bind 127.0.0.1 >/dev/null 2>&1 &)
sleep 1
"$ROOT/tools/release/homebrew-formula.sh" "$v" http://127.0.0.1:18600 "$DIST/SHA256SUMS" > "$DIST/veil.rb" || { bad "formula generation"; exit 1; }
step "brew tap-new" brew tap-new --no-git aofusa/localtest
tap="$(brew --repository)/Library/Taps/aofusa/homebrew-localtest"
cp "$DIST/veil.rb" "$tap/Formula/veil.rb"
step "brew style" brew style "$tap/Formula/veil.rb"
step "brew install" brew install --formula aofusa/localtest/veil || { echo "FAIL HOMEBREW TEST"; exit 1; }
step "brew test" brew test aofusa/localtest/veil
prefix="$(brew --prefix)"
if grep -q "$prefix/opt/veil/share/veil/www" "$prefix/etc/veil/config.toml"; then ok "etc/veil/config.toml points at the installed page"; else bad "etc/veil/config.toml: $(grep -n 'path =' "$prefix/etc/veil/config.toml" | head -3)"; fi
serve_check() {
    local i
    for i in $(seq 1 30); do curl -fsS -o /dev/null http://127.0.0.1:8080/ 2>/dev/null && break; sleep 1; done
    if curl -fsS http://127.0.0.1:8080/quickstart.html 2>/dev/null | grep -q '<title>veil quickstart</title>'; then ok "$1: the default page on 127.0.0.1:8080"; else bad "$1: no page on 127.0.0.1:8080"; fi
}
if [ "$(uname)" = Darwin ]; then
    step "brew services start" brew services start aofusa/localtest/veil
    serve_check "brew services"
    brew services stop aofusa/localtest/veil >/dev/null 2>&1
else
    # Linux の brew services は systemd のユーザーのセッションが要るので、wrapper をそのまま起動する
    veil > "$DIST/veil.log" 2>&1 & pid=$!
    serve_check "veil (no arguments)"
    kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
    grep -q "from VEIL_CONFIG" "$DIST/veil.log" && ok "the wrapper sets VEIL_CONFIG" || bad "the wrapper did not set VEIL_CONFIG: $(grep 'Config File' "$DIST/veil.log")"
fi
echo "# edited by test" >> "$prefix/etc/veil/config.toml"
step "brew reinstall" brew reinstall aofusa/localtest/veil
grep -q "edited by test" "$prefix/etc/veil/config.toml" && ok "reinstall keeps the edited configuration" || bad "reinstall overwrote the configuration"
step "brew uninstall" brew uninstall aofusa/localtest/veil
[ "$FAILS" = 0 ] && echo "HOMEBREW TEST: ALL OK" || { echo "HOMEBREW TEST: $FAILS failure(s)"; exit 1; }
