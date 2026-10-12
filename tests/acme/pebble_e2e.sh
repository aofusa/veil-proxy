#!/usr/bin/env bash
# ACME（F-05）の E2E: Pebble（Let's Encrypt のテスト用 ACME サーバ）から HTTP-01 で証明書を発行し、
# veil が仮の自己署名証明書から発行済み証明書へ無停止で切り替わることを確かめる。
#
#   cargo build --features full && tests/acme/pebble_e2e.sh [path/to/veil]
#
# 必要なもの: docker（ghcr.io/letsencrypt/pebble）、openssl、curl。Pebble はホストのネットワークで動かし、
# veil.localhost（= ループバック）の :5002 に HTTP-01 の検証を送る。
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
VEIL="${1:-$ROOT/target/debug/veil}"
# snap 版 docker は /tmp をマウントできないので target/ 配下に作る
mkdir -p "$ROOT/target/tmp"
WORK="$(mktemp -d -p "$ROOT/target/tmp")"
PEBBLE="veil-pebble-$$"
DOMAIN="veil.localhost"
PORT="${VEIL_ACME_E2E_PORT:-18443}"
VEIL_PID=""
cleanup() {
  [ -n "$VEIL_PID" ] && kill "$VEIL_PID" 2>/dev/null || true
  docker rm -f "$PEBBLE" >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

curl -sfo "$WORK/pebble.minica.pem" \
  https://raw.githubusercontent.com/letsencrypt/pebble/main/test/certs/pebble.minica.pem
cat > "$WORK/pebble.json" <<'JSON'
{"pebble": {"listenAddress": "0.0.0.0:14000", "managementListenAddress": "0.0.0.0:15000",
  "certificate": "test/certs/localhost/cert.pem", "privateKey": "test/certs/localhost/key.pem",
  "httpPort": 5002, "tlsPort": 5001, "ocspResponderURL": "", "externalAccountBindingRequired": false}}
JSON
docker run -d --name "$PEBBLE" --network host -e PEBBLE_VA_NOSLEEP=1 \
  -v "$WORK/pebble.json:/test/config/pebble-config.json:ro" \
  ghcr.io/letsencrypt/pebble:latest -config /test/config/pebble-config.json >/dev/null
for _ in $(seq 1 50); do curl -s --cacert "$WORK/pebble.minica.pem" https://localhost:14000/dir >/dev/null && break; sleep 0.2; done

mkdir -p "$WORK/www" "$WORK/tls"
echo "acme ok" > "$WORK/www/index.html"
cat > "$WORK/config.toml" <<TOML
[server]
listen = "127.0.0.1:$PORT"
threads = 1
[logging]
level = "info"
[tls]
cert_path = "$WORK/tls/cert.pem"
key_path = "$WORK/tls/key.pem"
reload_interval_secs = 1
[acme]
enabled = true
directory_url = "https://localhost:14000/dir"
directory_ca_cert = "$WORK/pebble.minica.pem"
contact = ["mailto:ops@example.com"]
domains = ["$DOMAIN"]
storage_dir = "$WORK/acme"
accept_terms_of_service = true
challenge_listen = "[::]:5002"
[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "$WORK/www"
TOML

"$VEIL" -c "$WORK/config.toml" > "$WORK/veil.log" 2>&1 &
VEIL_PID=$!
issuer() {
  echo | openssl s_client -connect "127.0.0.1:$PORT" -servername "$DOMAIN" 2>/dev/null \
    | openssl x509 -noout -issuer 2>/dev/null || true
}
for _ in $(seq 1 50); do curl -sk "https://127.0.0.1:$PORT/" >/dev/null && break; sleep 0.2; done
first="$(issuer)"
echo "initial certificate: $first"
case "$first" in *"$DOMAIN"*) echo "ok: temporary self-signed certificate served at startup";; *) echo "FAIL: no temporary certificate"; cat "$WORK/veil.log"; exit 1;; esac

ok=0
for _ in $(seq 1 60); do
  case "$(issuer)" in *Pebble*) ok=1; break;; esac
  sleep 1
done
[ "$ok" = 1 ] || { echo "FAIL: certificate not issued by Pebble"; tail -40 "$WORK/veil.log"; exit 1; }
echo "ok: certificate issued via HTTP-01 and hot-reloaded ($(issuer))"
body=$(curl -s --resolve "$DOMAIN:$PORT:127.0.0.1" -k "https://$DOMAIN:$PORT/")
[ "$body" = "acme ok" ] || { echo "FAIL: body '$body'"; exit 1; }
echo "ok: HTTPS serves content with the issued certificate"
[ -f "$WORK/acme/account.key" ] && [ ! -f "$WORK/acme/placeholder" ] || { echo "FAIL: storage state"; ls -la "$WORK/acme"; exit 1; }
perm=$(stat -c %a "$WORK/tls/key.pem"); [ "$perm" = 600 ] || { echo "FAIL: key mode $perm"; exit 1; }
echo "ok: account key stored, placeholder removed, private key mode 600"
curl -s -o /dev/null -w "%{http_code} %{redirect_url}\n" "http://127.0.0.1:5002/some/path" -H "Host: $DOMAIN" | grep -q "301 https://$DOMAIN/some/path" \
  && echo "ok: challenge listener redirects other paths to https"
echo PASS
