#!/usr/bin/env bash
# F-247: 展開したリリースのアーカイブ（veil-<ver>-<target>/）で、バイナリだけの利用者の手順が動くかを確かめる。
#
#   tools/release/binary-smoke.sh <unpacked-dir> <version> [port]
#
#   1. 設定ファイルが無いと、探したパスを示して終了コード 1 で止まる（設定は内蔵しない）
#   2. -c examples/config.toml で、/・/quickstart.html・h2c が 200、アクセスログが出る
#   3. VEIL_CONFIG=examples/config.toml でも同じ
#   4. --version がタグの版
# Linux・macOS・Windows（Git Bash）・FreeBSD・OpenBSD・NetBSD で同じものを回す。
set -uo pipefail

DIR="${1:?unpacked dir}"; VERSION="${2:?version}"; PORT="${3:-18089}"
cd "$DIR" || exit 1
BIN=./veil; [ -x ./veil.exe ] && BIN=./veil.exe
FAILS=0
ok() { echo "ok   $*"; }
bad() { echo "FAIL $*"; echo "::error title=binary smoke ($DIR)::$*"; FAILS=$((FAILS+1)); }
LOG="$(mktemp)"

# 例の設定のポートを試験用にずらす（ほかのジョブ・既定の 8080 とぶつけない）
sed "s/127.0.0.1:8080/127.0.0.1:$PORT/" examples/config.toml > examples/smoke.toml

v="$("$BIN" --version 2>&1)"
grep -q "$VERSION" <<<"$v" && ok "--version: $v" || bad "--version: '$v' (want $VERSION)"

# 1. 設定ファイルが無い（既定のパスに無い前提。VEIL_CONFIG も外す）
out="$(unset VEIL_CONFIG; "$BIN" 2>&1)"; rc=$?
if [ "$rc" = 1 ] && grep -q "configuration file not found" <<<"$out"; then ok "no configuration file: exit 1 with the path"
else bad "no configuration file: rc=$rc out=$(head -c 300 <<<"$out")"; fi

serve_check() {
    local label="$1"; shift
    "$@" > "$LOG" 2>&1 &
    local pid=$!
    local up=0
    local i=0
    while [ "$i" -lt 60 ]; do
        if curl -s -o /dev/null "http://127.0.0.1:$PORT/"; then up=1; break; fi
        sleep 1; i=$((i+1))
    done
    if [ "$up" = 1 ]; then
        for p in / /quickstart.html; do
            code="$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT$p")"
            [ "$code" = 200 ] && ok "$label: GET $p" || bad "$label: GET $p -> $code"
        done
        if curl -V 2>/dev/null | grep -qi 'HTTP2'; then
            code="$(curl -s --http2-prior-knowledge -o /dev/null -w '%{http_code} %{http_version}' "http://127.0.0.1:$PORT/")"
            [ "$code" = "200 2" ] && ok "$label: h2c" || bad "$label: h2c -> $code"
        else
            echo "skip $label: h2c (this curl has no HTTP/2)"
        fi
        code="$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/no-such-file")"
        [ "$code" = 404 ] && ok "$label: missing file is 404" || bad "$label: missing file -> $code"
    else
        bad "$label: did not start"; tail -30 "$LOG"
    fi
    kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
    if grep -q 'type=access' "$LOG"; then ok "$label: access log on stdout"; else bad "$label: no access log"; tail -20 "$LOG"; fi
}

# 2. -c
serve_check "-c examples/config.toml" "$BIN" -c examples/smoke.toml
grep -q 'from -c' "$LOG" && ok "-c: startup log names the source" || bad "-c: no 'from -c' in the startup log"
# 3. VEIL_CONFIG
serve_check "VEIL_CONFIG" env VEIL_CONFIG=examples/smoke.toml "$BIN"
grep -q 'from VEIL_CONFIG' "$LOG" && ok "VEIL_CONFIG: startup log names the source" || bad "VEIL_CONFIG: no 'from VEIL_CONFIG'"

rm -f examples/smoke.toml "$LOG"
if [ "$FAILS" -gt 0 ]; then echo "BINARY SMOKE: $FAILS failure(s)"; exit 1; fi
echo "BINARY SMOKE: ALL OK"
