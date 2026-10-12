#!/usr/bin/env bash
# compose.secrets.yaml の E2E（F-179 / F-180）。ビルド済みの veil イメージを使う。
#
#   VEIL_IMAGE=veil:musl examples/container/e2e.sh
#
# 1. Docker secrets で渡した証明書で TLS 応答する
# 2. VEIL_OVERRIDE_<n> の上書き（server.threads = 2）が効いている
# 3. secrets/ の証明書を置き換えると auto_reload で新しい証明書に替わる
# 4. docker stop（SIGTERM）で猶予内に終了する
set -euo pipefail
cd "$(dirname "$0")"
export VEIL_IMAGE="${VEIL_IMAGE:-veil:musl}"
PROJECT="veil-f180-$$"
compose() { docker compose -p "$PROJECT" -f compose.secrets.yaml "$@"; }
cleanup() { compose down -v --remove-orphans >/dev/null 2>&1 || true; rm -rf secrets www; }
trap cleanup EXIT

subject() {
  echo | openssl s_client -connect 127.0.0.1:8443 -servername localhost 2>/dev/null \
    | openssl x509 -noout -subject 2>/dev/null | sed 's/.*CN *= *//'
}

./gen-cert.sh v1.example >/dev/null
compose up -d
for _ in $(seq 1 60); do
  if curl -sk https://127.0.0.1:8443/ >/dev/null 2>&1; then break; fi
  sleep 0.5
done
body=$(curl -sk https://127.0.0.1:8443/)
[ "$body" = "hello from veil" ] || { echo "FAIL: unexpected body: $body"; compose logs; exit 1; }
[ "$(subject)" = "v1.example" ] || { echo "FAIL: initial certificate is not v1"; exit 1; }
echo "ok: TLS with Docker secrets (CN=v1.example)"

compose logs veil | grep -q "Workers: 2" || { echo "FAIL: VEIL_OVERRIDE_1 (threads=2) not applied"; compose logs; exit 1; }
echo "ok: VEIL_OVERRIDE_1 applied (Workers: 2)"

# 証明書を置き換える（Docker secrets の file: は bind mount なので inode を保ったまま上書きする）
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout /tmp/veil-f180-key.$$ -out /tmp/veil-f180-cert.$$ -days 30 -subj "/CN=v2.example" \
  -addext "subjectAltName=DNS:localhost" 2>/dev/null
cat /tmp/veil-f180-key.$$ > secrets/key.pem
cat /tmp/veil-f180-cert.$$ > secrets/cert.pem
rm -f /tmp/veil-f180-key.$$ /tmp/veil-f180-cert.$$
ok=0
for _ in $(seq 1 40); do
  if [ "$(subject)" = "v2.example" ]; then ok=1; break; fi
  sleep 1
done
[ "$ok" = 1 ] || { echo "FAIL: certificate was not reloaded"; compose logs; exit 1; }
echo "ok: certificate hot-reloaded from Docker secrets (CN=v2.example)"

start=$(date +%s)
compose stop -t 15 veil >/dev/null
elapsed=$(( $(date +%s) - start ))
code=$(docker inspect -f '{{.State.ExitCode}}' "$(compose ps -aq veil)")
[ "$elapsed" -lt 15 ] && [ "$code" = 0 ] || { echo "FAIL: graceful stop took ${elapsed}s exit=$code"; exit 1; }
echo "ok: graceful stop in ${elapsed}s (exit $code)"
echo "PASS"
