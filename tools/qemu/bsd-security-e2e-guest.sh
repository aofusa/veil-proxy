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
#   - SIGHUP で証明書をリロードできる（[tls] auto_reload。サンドボックス下でも新しい証明書を読める）
#   - netbsd: ワーカーが root 以外の利用者で動く
#   - freebsd: capability mode 下でも SIGHUP で設定を読み直せる（F-178）。cap_enter 後に動かない
#     変更（未登録の静的ルート・プロキシ）は拒否して前の設定を保つ。アクセスログを mv して
#     SIGHUP すると同じパスで開き直す
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
# 拡張を明示して X.509 v3 にする（OpenBSD の LibreSSL は拡張なしの `req -x509` で v1 を作り、
# rustls が UnsupportedCertVersion で拒否する）。
mkcert() {
  cat > "$DATA/req.cnf" <<REQ
[req]
distinguished_name = dn
x509_extensions = v3
prompt = no
[dn]
CN = localhost-$1
[v3]
subjectAltName = DNS:localhost
basicConstraints = CA:FALSE
REQ
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -config "$DATA/req.cnf" \
    -keyout "$DATA/key.pem.new" -out "$DATA/cert.pem.new" -days 3 >/dev/null 2>&1
  chmod 644 "$DATA/key.pem.new" "$DATA/cert.pem.new"
  mv "$DATA/key.pem.new" "$DATA/key.pem"; mv "$DATA/cert.pem.new" "$DATA/cert.pem"
}
mkcert 1

# F-178: FreeBSD はアクセスログも出す（capability mode 下の開き直しを確かめる）
ACCESS_LOG=""
if [ "$OS" = "freebsd" ]; then
  mkdir -p "$WORK/logs" "$DATA/other"
  echo "sec-other" > "$DATA/other/index.html"
  ACCESS_LOG="[access_log]
enabled = true
format = \"text\"
file_path = \"$WORK/logs/access.log\"
flush_interval_ms = 200"
fi

# 設定を書く（rename で置き換える。$1 = Server ヘッダの値、$2 = 先頭に足すルート）
write_cfg() {
  cat > "$WORK/veil.toml.new" <<CFG
[server]
listen = "127.0.0.1:$PORT"
threads = 2
server_header_enabled = true
server_header_value = "$1"
[tls]
cert_path = "$DATA/cert.pem"
key_path = "$DATA/key.pem"
# 証明書リロードは auto_reload の TLS リロードスレッド経由（SIGHUP で即時）。capsicum の
# capability mode でも、証明書は cap_enter 前に開いた dirfd 経由で読める（F-136。
# 設定ファイルの再読込も同じ仕組み。F-178）。
auto_reload = true
reload_interval_secs = 3600
[security]
$SEC
[logging]
level = "info"
$ACCESS_LOG
$2
[[route]]
[route.conditions]
host = "localhost"
path = "/*"
[route.action]
type = "File"
path = "$DATA/www"
index = "index.html"
CFG
  mv "$WORK/veil.toml.new" "$WORK/veil.toml"
}
write_cfg "veil-sec-1" ""

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

# F-178: capability mode 下の設定リロード（FreeBSD）
if [ "$OS" = "freebsd" ]; then
  srv() { curl -sk -D - -o /dev/null "https://localhost:$PORT/" | tr -d '\r' | sed -n 's/^[Ss]erver: //p'; }
  ROUTE_SUB='[[route]]
[route.conditions]
host = "localhost"
path = "/n2/*"
[route.action]
type = "File"
path = "'"$DATA"'/www/sub"'
  # 1. 受け入れ: Server ヘッダの変更と、登録済みルート配下のディレクトリを指すルートの追加
  write_cfg "veil-sec-2" "$ROUTE_SUB"; kill -HUP "$PID"; sleep 3
  s1=$(srv)
  n2_code=$(curl -sk -o r_n2.txt -w '%{http_code}' "https://localhost:$PORT/n2/nested.html" || echo 000)
  echo "RELOAD server=[$s1] n2 http=$n2_code body=[$(cat r_n2.txt 2>/dev/null)]"
  [ "$s1" = "veil-sec-2" ] || { echo "FAIL: config was not reloaded under capability mode"; ok=0; }
  [ "$n2_code" = "200" ] && grep -q sec-nested-ok r_n2.txt || { echo "FAIL: route added by reload"; ok=0; }
  # 2. 拒否: cap_enter 前に登録していない静的ルート
  write_cfg "veil-sec-3" '[[route]]
[route.conditions]
host = "localhost"
path = "/other/*"
[route.action]
type = "File"
path = "'"$DATA"'/other"'
  kill -HUP "$PID"; sleep 3
  s2=$(srv)
  echo "REJECT-ROOT server=[$s2]"
  [ "$s2" = "veil-sec-2" ] || { echo "FAIL: unregistered static root was not rejected"; ok=0; }
  grep -q 'was not registered before cap_enter' "$WORK/veil.log" || { echo "FAIL: no rejection log (root)"; ok=0; }
  # 3. 拒否: プロキシルート（connect(2) が要る）
  write_cfg "veil-sec-4" '[[route]]
[route.conditions]
host = "localhost"
path = "/api/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:9"'
  kill -HUP "$PID"; sleep 3
  s3=$(srv)
  echo "REJECT-PROXY server=[$s3]"
  [ "$s3" = "veil-sec-2" ] || { echo "FAIL: proxy route was not rejected"; ok=0; }
  grep -q 'Proxy/ProxyUpstream route requires connect' "$WORK/veil.log" || { echo "FAIL: no rejection log (proxy)"; ok=0; }
  # 4. アクセスログの開き直し（logrotate の move + SIGHUP）
  write_cfg "veil-sec-2" "$ROUTE_SUB"
  mv "$WORK/logs/access.log" "$WORK/logs/access.log.1"
  kill -HUP "$PID"; sleep 3
  curl -sk -o /dev/null "https://localhost:$PORT/after-rotate.html"; sleep 2
  echo "ACCESS-LOG new=[$(grep -c after-rotate "$WORK/logs/access.log" 2>/dev/null)] old=[$(grep -c after-rotate "$WORK/logs/access.log.1" 2>/dev/null)]"
  grep -q after-rotate "$WORK/logs/access.log" 2>/dev/null || { echo "FAIL: access log was not reopened"; ok=0; }
  s4=$(srv)
  [ "$s4" = "veil-sec-2" ] || { echo "FAIL: reload after rotation ($s4)"; ok=0; }
fi

if [ "$OS" = "netbsd" ]; then
  owner=$(ps -o user= -p "$PID" 2>/dev/null | tr -d ' ')
  echo "PROCESS user=[$owner]"
  [ "$owner" = "$USER_NAME" ] || { echo "FAIL: privileges were not dropped"; ok=0; }
fi

kill "$PID" 2>/dev/null; sleep 1
if grep -q 'panicked at' "$WORK/veil.log"; then echo "FAIL: panic"; grep -A5 'panicked at' "$WORK/veil.log"; ok=0; fi
grep -iE 'capability mode|pledge|unveil|chroot|privilege' "$WORK/veil.log" | head -12
[ "$OS" = "netbsd" ] && rm -f "$DATA"
if [ "$ok" = "1" ]; then echo "SECURITY_E2E=PASS"; exit 0; fi
echo "SECURITY_E2E=FAIL"; tail -20 "$WORK/veil.log"; exit 1
