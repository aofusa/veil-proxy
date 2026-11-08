#!/usr/bin/env bash
# =============================================================================
# ktls_h2_repro.sh — kTLS + CBPF + HTTP/2 の回帰の手順（B-75 の再現手順。F-297）
# =============================================================================
#
# 同じサーバへ wrk（HTTP/1.1）を 3 回流してから h2load（HTTP/2）を 3 回流す。B-75 はこの順で h2load が止まり、
# 単体・統合・E2E は全部通った。reactor の待ち方（武装・確認・起こす順番）を変えたら、epoll と io_uring の両方で回す。
#
# 使い方（Linux、kTLS のモジュールが読めること。wrk・h2load・curl・openssl が要る）:
#   bash tools/perf/ktls_h2_repro.sh target/release/veil
#   SERVER_CPUS=0-3 LOAD_CPUS=4-7 bash tools/perf/ktls_h2_repro.sh ./veil-epoll
#
# h2load が時間内に終わらない・失敗が出る・kTLS が有効にならない、のどれかで 0 以外で終わる。
set -u

BIN="${1:?veil のバイナリを指定する}"
SERVER_CPUS="${SERVER_CPUS:-0-7}"
LOAD_CPUS="${LOAD_CPUS:-8-15}"
PORT="${PORT:-18443}"
H2_REQUESTS="${H2_REQUESTS:-100000}"

for t in wrk h2load curl openssl taskset; do
    command -v "$t" >/dev/null || { echo "$t が無い" >&2; exit 2; }
done
ulimit -n 100000 2>/dev/null

W=$(mktemp -d)
P=""
cleanup() {
    [ -n "$P" ] && kill "$P" 2>/dev/null && wait "$P" 2>/dev/null
    rm -rf "$W"
}
trap cleanup EXIT

mkdir -p "$W/www"
head -c 54576 /dev/zero | tr '\0' x > "$W/www/index.html"
openssl req -x509 -newkey rsa:2048 -nodes -keyout "$W/key.pem" -out "$W/cert.pem" -days 2 \
    -subj /CN=localhost 2>/dev/null || { echo "証明書を作れない" >&2; exit 2; }
cat > "$W/config.toml" <<EOF
[server]
listen = "127.0.0.1:$PORT"
http2_enabled = true
threads = 8

[logging]
level = "info"

[performance]
huge_pages_enabled = false
reuseport_balancing = "cbpf"

[tls]
cert_path = "$W/cert.pem"
key_path = "$W/key.pem"
ktls_enabled = true
ktls_fallback_enabled = true

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "$W/www/"
[route.security]
allowed_methods = ["HEAD", "GET"]
EOF

taskset -c "$SERVER_CPUS" "$BIN" -c "$W/config.toml" > "$W/server.log" 2>&1 &
P=$!
for _ in $(seq 50); do
    curl -sk -o /dev/null "https://127.0.0.1:$PORT/index.html" && break
    sleep 0.1
done

URL="https://127.0.0.1:$PORT/index.html"
fail=0
for i in 1 2 3; do
    echo "wrk[$i] $(taskset -c "$LOAD_CPUS" wrk -t4 -c64 -d3s "$URL" | grep -E "Requests/sec|Socket errors|Non-2xx" | tr -s ' ' | tr '\n' ' ')"
done
for i in 1 2 3; do
    out=$(taskset -c "$LOAD_CPUS" timeout 30 h2load -t4 -c32 -m10 -n "$H2_REQUESTS" "$URL" 2>&1)
    rc=$?
    echo "h2load[$i] exit=$rc $(echo "$out" | grep -E "finished in|^requests:" | tr '\n' ' ')"
    echo "$out" | grep -q "^requests: $H2_REQUESTS total, $H2_REQUESTS started, $H2_REQUESTS done, $H2_REQUESTS succeeded" \
        || fail=1
done
code=$(curl -sk --http2 -o /dev/null -w "%{http_code} %{size_download}" "$URL")
echo "curl h2: $code"
[ "$code" = "200 54576" ] || fail=1

kill "$P" 2>/dev/null
wait "$P" 2>/dev/null
P=""
if grep -q "kTLS: Enabled" "$W/server.log"; then
    grep "kTLS: Enabled" "$W/server.log" | head -1 | cut -c1-160
else
    echo "kTLS が有効になっていない（modprobe tls を確かめる）:" >&2
    grep -i ktls "$W/server.log" | head -3 >&2
    fail=1
fi
[ "$fail" = 0 ] && echo "OK" || echo "FAILED"
exit "$fail"
