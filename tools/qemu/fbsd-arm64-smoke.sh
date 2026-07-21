#!/bin/sh
# FreeBSD arm64 VM 内: ビルド済み veil で HTTPS 静的配信 smoke（fbsd-arm64-vm.sh smoke が実行）。
set -e
BIN="${VEIL_BIN:-/root/veil-proxy/target/release/veil}"
PORT="${VEIL_PORT:-9443}"
WORK=/root/arm_smoke
rm -rf $WORK; mkdir -p $WORK/www
echo "veil-freebsd-aarch64-ok" > $WORK/www/index.html
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:secp384r1 -nodes \
  -keyout $WORK/key.pem -out $WORK/cert.pem -days 3 -subj "/CN=localhost" >/dev/null 2>&1
cat > $WORK/veil.toml <<CFG
[server]
listen = "127.0.0.1:$PORT"
threads = 2
[tls]
cert_path = "$WORK/cert.pem"
key_path = "$WORK/key.pem"
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
cd $WORK
nohup $BIN --config veil.toml > veil.log 2>&1 &
sleep 5
echo "--- guest ---"; uname -mr
echo "--- runtime log ---"; grep -iE 'reactor|kqueue|Worker started|Listen' veil.log | head -6 || true
hc=$(curl -sk -o /tmp/b.txt -w '%{http_code}' https://localhost:$PORT/ || echo 000)
echo "HTTP_CODE=$hc BODY=$(cat /tmp/b.txt 2>/dev/null)"
if [ "$hc" = "200" ] && grep -q 'veil-freebsd-aarch64-ok' /tmp/b.txt; then echo "ARM_SMOKE=PASS"; else echo "ARM_SMOKE=FAIL"; cat veil.log; fi
pkill -x veil 2>/dev/null || true
