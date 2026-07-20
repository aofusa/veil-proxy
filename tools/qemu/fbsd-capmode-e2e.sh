#!/bin/sh
# F-123 capsicum capability mode 静的配信 E2E（FreeBSD VM 内で実行）
# =================================================================
#
# FreeBSD の capability mode（cap_enter(2)）下で静的ファイル配信が完全動作すること
# （dirfd + openat/fstatat + O_RESOLVE_BENEATH 相対化）を検証する。
#
# 前提: FreeBSD VM 内に veil ソースを配置し `cargo build --release` 済みであること
#       （tools/qemu の想定どおり、ホストでクロス転送 or VM 内ビルド）。
# 使い方（VM 内）: sh fbsd-capmode-e2e.sh
#
# 期待結果:
#   - ルート/ネストした静的ファイルが HTTP 200 で配信される（従来は cap-mode で 404）
#   - パストラバーサル（../../secret）は 403（O_RESOLVE_BENEATH が拒否）
#   - "capability mode active" ログが出る／panic が発生しない
set -e
BIN="${VEIL_BIN:-/root/veil-proxy/target/release/veil}"
PORT="${VEIL_PORT:-9443}"
WORK="${WORK:-/root/capmode_e2e}"
rm -rf "$WORK"; mkdir -p "$WORK/www/sub"
echo "capmode-root-ok" > "$WORK/www/index.html"
echo "capmode-nested-ok" > "$WORK/www/sub/nested.html"
echo "SECRET-must-not-leak" > /root/capmode_secret.txt
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:secp384r1 -nodes \
  -keyout "$WORK/key.pem" -out "$WORK/cert.pem" -days 3 -subj "/CN=localhost" >/dev/null 2>&1

cat > "$WORK/veil.toml" <<CFG
[server]
listen = "127.0.0.1:$PORT"
threads = 2
[tls]
cert_path = "$WORK/cert.pem"
key_path = "$WORK/key.pem"
[security]
enable_capsicum = true
capsicum_capability_mode = true
[logging]
level = "info"
[[route]]
[route.conditions]
host = "localhost"
path = "/*"
[route.action]
type = "File"
path = "$WORK/www"
index = "index.html"
CFG

pkill -x veil 2>/dev/null || true; sleep 1
cd "$WORK"
env RUST_BACKTRACE=1 nohup "$BIN" --config veil.toml > veil.log 2>&1 &
sleep 6

echo "===== capability mode log ====="
grep -iE 'capability mode|静的ルート dirfd|cap_enter|capsicum' veil.log | head -10 || true

root_code=$(curl -sk -o r_root.txt -w '%{http_code}' "https://localhost:$PORT/" || echo 000)
nested_code=$(curl -sk -o r_nest.txt -w '%{http_code}' "https://localhost:$PORT/sub/nested.html" || echo 000)
trav_code=$(curl -sk --path-as-is -o r_trav.txt -w '%{http_code}' "https://localhost:$PORT/../../capmode_secret.txt" || echo 000)

echo "ROOT   http=$root_code body=[$(cat r_root.txt 2>/dev/null)]"
echo "NESTED http=$nested_code body=[$(cat r_nest.txt 2>/dev/null)]"
echo "TRAV   http=$trav_code leaked=$(grep -c SECRET r_trav.txt 2>/dev/null)"

ok=1
[ "$root_code" = "200" ] && grep -q capmode-root-ok r_root.txt || { echo "FAIL: root serving"; ok=0; }
[ "$nested_code" = "200" ] && grep -q capmode-nested-ok r_nest.txt || { echo "FAIL: nested serving"; ok=0; }
grep -q SECRET r_trav.txt 2>/dev/null && { echo "FAIL: traversal leaked secret"; ok=0; }
[ "$(grep -c 'panicked at' veil.log)" = "0" ] || { echo "FAIL: panic detected"; ok=0; }
grep -qiE 'capability mode active|entered capability mode' veil.log || echo "WARN: cap_enter log not found"
[ "$ok" = "1" ] && echo "F123_E2E=PASS" || echo "F123_E2E=FAIL"
pkill -x veil 2>/dev/null || true
