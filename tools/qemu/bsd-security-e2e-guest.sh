#!/bin/sh
# OS 固有のサンドボックス下での E2E（F-176 N7。VM 内で実行する）
# =================================================================
#
# 使い方（VM 内）: sh bsd-security-e2e-guest.sh <freebsd|openbsd|netbsd> <veil バイナリ>
# ホストからは tools/qemu/bsd-security-e2e.sh <os> <arch>（= bsd-vm.sh <os> <arch> security-e2e）。
#
# OS ごとのサンドボックス:
#   freebsd: capsicum の capability mode（F-123。dirfd + openat の静的配信）
#   openbsd: pledge + unveil（F-120 Phase 5）
#   netbsd : chroot + 特権降格（F-140。pledge/unveil 相当の API が無いため）
#
# 共通の確認:
#   - ルート / ネストした静的ファイルが 200 で返る
#   - パストラバーサルで静的ルートの外のファイルが漏れない
#   - panic しない
#   - SIGHUP で証明書をリロードできる（サンドボックス下でも新しい証明書を読める）
#   - netbsd: ワーカーが root 以外の利用者で動く
set -u
OS="${1:?os}"; BIN="${2:?veil binary}"
PORT="${VEIL_PORT:-9443}"
WORK="${WORK:-/var/tmp/veil-sec-e2e}"
SECRET="/var/tmp/veil-sec-e2e-secret.txt"

pkill -x veil 2>/dev/null; sleep 1
rm -rf "$WORK"; mkdir -p "$WORK"
echo "SECRET-must-not-leak" > "$SECRET"

case "$OS" in
  netbsd)
    # chroot 後も、chroot 前の（起動時の）読み込みでも同じパスで解決できるよう、
    # 実体を chroot 内に置き、chroot 外の同じパスにはシンボリックリンクを張る。
    CH="$WORK/chroot"
    DATA="/veil-sec-e2e"
    mkdir -p "$CH$DATA/www/sub"
    rm -f "$DATA"; ln -s "$CH$DATA" "$DATA"
    USER_NAME=nobody
    GROUP_NAME=$(id -gn nobody 2>/dev/null || echo nogroup)
    SEC="chroot_dir = \"$CH\"
drop_privileges_user = \"$USER_NAME\"
drop_privileges_group = \"$GROUP_NAME\""
    ;;
  openbsd)
    DATA="$WORK/data"; mkdir -p "$DATA/www/sub"
    SEC="enable_unveil = true
enable_pledge = true"
    ;;
  freebsd)
    DATA="$WORK/data"; mkdir -p "$DATA/www/sub"
    SEC="enable_capsicum = true
capsicum_capability_mode = true"
    ;;
  *) echo "unknown os: $OS"; exit 2 ;;
esac

echo "sec-root-ok" > "$DATA/www/index.html"
echo "sec-nested-ok" > "$DATA/www/sub/nested.html"
mkcert() {
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
    -keyout "$DATA/key.pem.new" -out "$DATA/cert.pem.new" -days 3 -subj "/CN=localhost-$1" >/dev/null 2>&1
  chmod 644 "$DATA/key.pem.new" "$DATA/cert.pem.new"
  mv "$DATA/key.pem.new" "$DATA/key.pem"; mv "$DATA/cert.pem.new" "$DATA/cert.pem"
}
mkcert 1

cat > "$WORK/veil.toml" <<CFG
[server]
listen = "127.0.0.1:$PORT"
threads = 2
[tls]
cert_path = "$DATA/cert.pem"
key_path = "$DATA/key.pem"
[security]
$SEC
[logging]
level = "info"
[[route]]
[route.conditions]
host = "localhost"
path = "/*"
[route.action]
type = "File"
path = "$DATA/www"
index = "index.html"
CFG

cd "$WORK"
env RUST_BACKTRACE=1 "$BIN" --config "$WORK/veil.toml" > "$WORK/veil.log" 2>&1 &
PID=$!
up=0
for i in $(seq 1 60); do
  if curl -sk -o /dev/null "https://localhost:$PORT/"; then up=1; break; fi
  sleep 1
done
fp() { openssl s_client -connect "127.0.0.1:$PORT" -servername localhost </dev/null 2>/dev/null \
  | openssl x509 -noout -subject 2>/dev/null; }

ok=1
[ "$up" = "1" ] || { echo "FAIL: veil did not come up"; tail -30 "$WORK/veil.log"; ok=0; }
root_code=$(curl -sk -o r_root.txt -w '%{http_code}' "https://localhost:$PORT/" || echo 000)
nest_code=$(curl -sk -o r_nest.txt -w '%{http_code}' "https://localhost:$PORT/sub/nested.html" || echo 000)
trav_code=$(curl -sk --path-as-is -o r_trav.txt -w '%{http_code}' \
  "https://localhost:$PORT/../../../../var/tmp/veil-sec-e2e-secret.txt" || echo 000)
echo "ROOT   http=$root_code body=[$(cat r_root.txt 2>/dev/null)]"
echo "NESTED http=$nest_code body=[$(cat r_nest.txt 2>/dev/null)]"
echo "TRAV   http=$trav_code"
[ "$root_code" = "200" ] && grep -q sec-root-ok r_root.txt || { echo "FAIL: root serving"; ok=0; }
[ "$nest_code" = "200" ] && grep -q sec-nested-ok r_nest.txt || { echo "FAIL: nested serving"; ok=0; }
grep -q SECRET r_trav.txt 2>/dev/null && { echo "FAIL: traversal leaked the secret"; ok=0; }

# 証明書のリロード（SIGHUP）
before=$(fp); mkcert 2; kill -HUP "$PID"; sleep 4; after=$(fp)
echo "CERT before=[$before] after=[$after]"
case "$after" in *localhost-2*) ;; *) echo "FAIL: certificate was not reloaded"; ok=0 ;; esac
code2=$(curl -sk -o /dev/null -w '%{http_code}' "https://localhost:$PORT/" || echo 000)
[ "$code2" = "200" ] || { echo "FAIL: serving after reload ($code2)"; ok=0; }

if [ "$OS" = "netbsd" ]; then
  owner=$(ps -o user= -p "$PID" 2>/dev/null | tr -d ' ')
  echo "PROCESS user=[$owner]"
  [ "$owner" = "$USER_NAME" ] || { echo "FAIL: privileges were not dropped"; ok=0; }
fi

kill "$PID" 2>/dev/null; sleep 1
if grep -q 'panicked at' "$WORK/veil.log"; then echo "FAIL: panic"; grep -A5 'panicked at' "$WORK/veil.log"; ok=0; fi
grep -iE 'capability mode|pledge|unveil|chroot|privilege' "$WORK/veil.log" | head -8
[ "$OS" = "netbsd" ] && rm -f "$DATA"
if [ "$ok" = "1" ]; then echo "SECURITY_E2E=PASS"; exit 0; fi
echo "SECURITY_E2E=FAIL"; tail -20 "$WORK/veil.log"; exit 1
