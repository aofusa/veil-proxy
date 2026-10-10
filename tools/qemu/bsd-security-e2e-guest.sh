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
#   - freebsd: capability mode 下でも接続ブローカー経由で上流へプロキシできる（F-182。TCP・ホスト名・
#     UDS・upstream グループ + ヘルスチェック）。許可リスト外の上流を足すリロードは拒否し、ブローカーが
#     死んだら本体も終了コード 1 で終わる
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

# 設定を書く（rename で置き換える。$1 = 先頭に足すルート）
write_cfg() {
  cat > "$WORK/veil.toml.new" <<CFG
[server]
listen = "127.0.0.1:$PORT"
threads = 2
${SERVER_EXTRA:-}
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
$1
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
write_cfg ""

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
# 反映の目印は /n2/*（www/sub を指す追加ルート）。拒否される設定はこのルートを含まないので、
# 誤って受け入れると /n2/ が 404 になる。
if [ "$OS" = "freebsd" ]; then
  n2() { curl -sk -o r_n2.txt -w '%{http_code}' "https://localhost:$PORT/n2/nested.html" || echo 000; }
  ROUTE_SUB='[[route]]
[route.conditions]
host = "localhost"
path = "/n2/*"
[route.action]
type = "File"
path = "'"$DATA"'/www/sub"'
  # 1. 受け入れ: 登録済みルート配下のディレクトリを指すルートの追加
  write_cfg "$ROUTE_SUB"; kill -HUP "$PID"; sleep 3
  c1=$(n2)
  echo "RELOAD n2 http=$c1 body=[$(cat r_n2.txt 2>/dev/null)]"
  [ "$c1" = "200" ] && grep -q sec-nested-ok r_n2.txt || { echo "FAIL: config was not reloaded under capability mode"; ok=0; }
  # 2. 拒否: cap_enter 前に登録していない静的ルート
  write_cfg '[[route]]
[route.conditions]
host = "localhost"
path = "/other/*"
[route.action]
type = "File"
path = "'"$DATA"'/other"'
  kill -HUP "$PID"; sleep 3
  c2=$(n2)
  oc=$(curl -sk -o r_other.txt -w '%{http_code}' "https://localhost:$PORT/other/index.html" || echo 000)
  echo "REJECT-ROOT n2 http=$c2 other http=$oc"
  [ "$c2" = "200" ] || { echo "FAIL: unregistered static root was not rejected"; ok=0; }
  grep -q sec-other r_other.txt 2>/dev/null && { echo "FAIL: unregistered root was served"; ok=0; }
  grep -q 'was not registered before cap_enter' "$WORK/veil.log" || { echo "FAIL: no rejection log (root)"; ok=0; }
  # 3. 拒否: プロキシルート（connect(2) が要る）
  write_cfg '[[route]]
[route.conditions]
host = "localhost"
path = "/api/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:9"'
  kill -HUP "$PID"; sleep 3
  c3=$(n2)
  echo "REJECT-PROXY n2 http=$c3"
  [ "$c3" = "200" ] || { echo "FAIL: proxy route was not rejected"; ok=0; }
  grep -q 'is not in the connect allowlist fixed at startup' "$WORK/veil.log" || { echo "FAIL: no rejection log (proxy)"; ok=0; }
  # 4. アクセスログの開き直し（logrotate の move + SIGHUP）
  write_cfg "$ROUTE_SUB"
  mv "$WORK/logs/access.log" "$WORK/logs/access.log.1"
  kill -HUP "$PID"; sleep 3
  curl -sk -o /dev/null "https://localhost:$PORT/after-rotate.html"; sleep 2
  echo "ACCESS-LOG new=[$(grep -c after-rotate "$WORK/logs/access.log" 2>/dev/null)] old=[$(grep -c after-rotate "$WORK/logs/access.log.1" 2>/dev/null)]"
  grep -q after-rotate "$WORK/logs/access.log" 2>/dev/null || { echo "FAIL: access log was not reopened"; ok=0; }
  c4=$(n2)
  [ "$c4" = "200" ] || { echo "FAIL: serving after rotation ($c4)"; ok=0; }
  grep -E 'Failed to reload configuration' "$WORK/veil.log" | sed 's/^.*Failed/Failed/' | head -4
fi

if [ "$OS" = "netbsd" ]; then
  owner=$(ps -o user= -p "$PID" 2>/dev/null | tr -d ' ')
  echo "PROCESS user=[$owner]"
  [ "$owner" = "$USER_NAME" ] || { echo "FAIL: privileges were not dropped"; ok=0; }
fi

kill "$PID" 2>/dev/null; sleep 1

# F-182: capability mode 下でも接続ブローカー経由で上流へプロキシできる。
if [ "$OS" = "freebsd" ]; then
  BK="$WORK/backend"; mkdir -p "$BK/www/up" "$BK/www/up2" "$BK/www/host" "$BK/www/grp" "$BK/www/uds"
  for d in up up2 host grp uds; do echo "backend-$d" > "$BK/www/$d/index.html"; done
  # ルートのパス接頭辞（/up 等）は外して上流へ転送されるので、上流のルートにも置く。
  echo "backend-root" > "$BK/www/index.html"
  bk_cfg() {
    cat > "$BK/$2.toml" <<B
[server]
listen = "$1"
threads = 1
[tls]
cert_path = "$DATA/cert.pem"
key_path = "$DATA/key.pem"
[logging]
level = "warn"
[[route]]
[route.conditions]
path = "/*"
[route.action]
type = "File"
path = "$BK/www"
index = "index.html"
B
  }
  BSOCK="$WORK/backend.sock"
  bk_cfg "127.0.0.1:18443" tcp
  bk_cfg "unix:$BSOCK" uds
  "$BIN" --config "$BK/tcp.toml" > "$BK/tcp.log" 2>&1 &
  BPID1=$!
  "$BIN" --config "$BK/uds.toml" > "$BK/uds.log" 2>&1 &
  BPID2=$!
  for i in $(seq 1 30); do
    curl -sk -o /dev/null "https://127.0.0.1:18443/up/index.html" \
      && curl -sk -o /dev/null --unix-socket "$BSOCK" "https://localhost/uds/index.html" && break
    sleep 1
  done
  # 上流は TLS（自己署名なので tls_insecure）。IP・ホスト名・UDS の 3 種類と、ヘルスチェック付きのグループ。
  UPSTREAMS='[upstreams.ip]
servers = ["https://127.0.0.1:18443"]
tls_insecure = true
[upstreams.host]
servers = ["https://localhost:18443"]
tls_insecure = true
[upstreams.uds]
servers = ["https://unix:'"$BSOCK"'"]
tls_insecure = true
[upstreams.pool]
servers = ["https://127.0.0.1:18443"]
tls_insecure = true
[upstreams.pool.health_check]
check_type = "http"
interval_secs = 1
path = "/grp/index.html"
timeout_secs = 2
healthy_statuses = [200]
use_tls = true
verify_cert = false
[[route]]
[route.conditions]
host = "localhost"
path = "/up/*"
[route.action]
type = "Proxy"
upstream = "ip"
[[route]]
[route.conditions]
host = "localhost"
path = "/host/*"
[route.action]
type = "Proxy"
upstream = "host"
[[route]]
[route.conditions]
host = "localhost"
path = "/uds/*"
[route.action]
type = "Proxy"
upstream = "uds"
[[route]]
[route.conditions]
host = "localhost"
path = "/grp/*"
[route.action]
type = "Proxy"
upstream = "pool"'
  write_cfg "$UPSTREAMS"
  "$BIN" --config "$WORK/veil.toml" > "$WORK/veil-br.log" 2>&1 &
  BRPID=$!
  for i in $(seq 1 60); do
    curl -sk -o /dev/null "https://localhost:$PORT/" && break; sleep 1
  done
  grep -q 'capability mode active' "$WORK/veil-br.log" || { echo "FAIL: capability mode was not entered with upstreams"; ok=0; }
  # ヘルスチェックがブローカー経由で通り、upstream が healthy のままであることを見るため少し待つ
  sleep 4
  for d in up host uds grp; do
    c=$(curl -sk -o "r_$d.txt" -w '%{http_code}' "https://localhost:$PORT/$d/index.html" || echo 000)
    echo "BROKER $d http=$c body=[$(cat "r_$d.txt" 2>/dev/null)]"
    [ "$c" = "200" ] && grep -q "backend-" "r_$d.txt" || { echo "FAIL: proxy via connect broker ($d)"; ok=0; }
  done
  # ブローカーは本体と同じ利用者で別プロセスとして動く
  bpid=$(pgrep -P "$BRPID" 2>/dev/null | head -1)
  echo "BROKER pid=[$bpid] user=[$(ps -o user= -p "$bpid" 2>/dev/null | tr -d ' ')] main_user=[$(ps -o user= -p "$BRPID" | tr -d ' ')]"
  [ -n "$bpid" ] || { echo "FAIL: connect broker process not found"; ok=0; }
  # リロード: 同じ上流のままの変更は通り、許可リスト外の上流を足すと拒否される
  write_cfg "$UPSTREAMS
[[route]]
[route.conditions]
host = \"localhost\"
path = \"/up2/*\"
[route.action]
type = \"Proxy\"
upstream = \"ip\""
  kill -HUP "$BRPID"; sleep 3
  c=$(curl -sk -o r_up2.txt -w '%{http_code}' "https://localhost:$PORT/up2/index.html" || echo 000)
  echo "BROKER reload-same-target http=$c body=[$(cat r_up2.txt 2>/dev/null)]"
  [ "$c" = "200" ] && grep -q backend- r_up2.txt || { echo "FAIL: reload with an allowed upstream"; ok=0; }
  write_cfg "$UPSTREAMS
[[route]]
[route.conditions]
host = \"localhost\"
path = \"/new/*\"
[route.action]
type = \"Proxy\"
url = \"http://127.0.0.1:18081\""
  kill -HUP "$BRPID"; sleep 3
  grep -q 'is not in the connect allowlist fixed at startup' "$WORK/veil-br.log" || { echo "FAIL: new upstream target was not rejected"; ok=0; }
  c=$(curl -sk -o /dev/null -w '%{http_code}' "https://localhost:$PORT/up/index.html" || echo 000)
  [ "$c" = "200" ] || { echo "FAIL: serving after rejected reload ($c)"; ok=0; }
  # ブローカーが死んだら本体も終了コード 1 で終わる（fail-closed）
  if [ -n "$bpid" ]; then
    kill -9 "$bpid"
    for i in $(seq 1 10); do kill -0 "$BRPID" 2>/dev/null || break; sleep 1; done
    if kill -0 "$BRPID" 2>/dev/null; then
      echo "FAIL: main process survived the broker"; kill "$BRPID"; ok=0
    else
      wait "$BRPID"; brc=$?
      echo "BROKER-DEATH exit=$brc"
      [ "$brc" = "1" ] || { echo "FAIL: unexpected exit status after broker death ($brc)"; ok=0; }
    fi
  else
    kill "$BRPID" 2>/dev/null
  fi
  grep -q 'panicked at' "$WORK/veil-br.log" && { echo "FAIL: panic (broker)"; grep -A5 'panicked at' "$WORK/veil-br.log"; ok=0; }
  kill "$BPID1" "$BPID2" 2>/dev/null; sleep 1
fi
# F-181: capability mode を要求したのに入れない構成（HTTP リダイレクトリスナー。cap_enter 後の
# bind が要る）は起動を中止する。allow_security_failures = true なら警告して rights 制限のみで起動する。
if [ "$OS" = "freebsd" ]; then
  SERVER_EXTRA='http = "127.0.0.1:9080"'
  write_cfg ""
  "$BIN" --config "$WORK/veil.toml" > "$WORK/veil-fc.log" 2>&1 &
  FPID=$!
  for i in $(seq 1 20); do kill -0 "$FPID" 2>/dev/null || break; sleep 1; done
  if kill -0 "$FPID" 2>/dev/null; then
    echo "FAIL: capability mode + redirect listener did not abort"; kill "$FPID"; ok=0
  else
    wait "$FPID"; frc=$?
    echo "FAIL-CLOSED exit=$frc"
    [ "$frc" = "1" ] || { echo "FAIL: unexpected exit status ($frc)"; ok=0; }
    grep -q 'aborting startup' "$WORK/veil-fc.log" || { echo "FAIL: no abort reason"; tail -5 "$WORK/veil-fc.log"; ok=0; }
  fi
  SEC_SAVED="$SEC"
  SEC="$SEC
allow_security_failures = true"
  write_cfg ""
  "$BIN" --config "$WORK/veil.toml" > "$WORK/veil-fo.log" 2>&1 &
  OPID=$!
  oc=000
  for i in $(seq 1 30); do
    oc=$(curl -sk -o /dev/null -w '%{http_code}' "https://localhost:$PORT/" || echo 000)
    [ "$oc" = "200" ] && break; sleep 1
  done
  echo "ALLOW-FAILURES http=$oc"
  [ "$oc" = "200" ] || { echo "FAIL: allow_security_failures = true did not start"; tail -5 "$WORK/veil-fo.log"; ok=0; }
  grep -q 'continuing in rights-limited mode' "$WORK/veil-fo.log" || { echo "FAIL: no rights-limited warning"; ok=0; }
  kill "$OPID" 2>/dev/null; sleep 1
  grep -q 'panicked at' "$WORK/veil-fc.log" "$WORK/veil-fo.log" && { echo "FAIL: panic (fail-closed)"; ok=0; }
  SEC="$SEC_SAVED"; SERVER_EXTRA=""
fi
if grep -q 'panicked at' "$WORK/veil.log"; then echo "FAIL: panic"; grep -A5 'panicked at' "$WORK/veil.log"; ok=0; fi
grep -iE 'capability mode|pledge|unveil|chroot|privilege' "$WORK/veil.log" | head -12
[ "$OS" = "netbsd" ] && rm -f "$DATA"
if [ "$ok" = "1" ]; then echo "SECURITY_E2E=PASS"; exit 0; fi
echo "SECURITY_E2E=FAIL"; tail -20 "$WORK/veil.log"; exit 1
