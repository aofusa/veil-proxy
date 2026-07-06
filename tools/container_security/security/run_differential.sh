#!/usr/bin/env bash
# F-77: プロトコル差分（differential）テスト。
# 同一のバックエンドを Veil と nginx の双方でフロントし、同じ crafted リクエスト群を
# 両者へ送ってステータス・フレーミング解釈の差分を比較する。曖昧な HTTP 解釈
# （リクエストスマグリングの温床）を検出する。専用トポロジを立てる自己完結スクリプト。
# docker のみで完結。既定 SKIP。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
RESULTS_DIR="${RESULTS_DIR:-${REPO_ROOT}/tools/container_security/results}"
DOCKER_DIR="${REPO_ROOT}/docker"
NET_NAME="${NET_NAME:-veil-sec-test-net}"
VEIL_IMAGE="${VEIL_IMAGE:-veil:glibc}"
SECCOMP="${DOCKER_DIR}/assets/security/seccomp.json"
NGINX_IMAGE="${NGINX_IMAGE:-nginx:alpine}"
HARNESS_IMAGE="${HARNESS_IMAGE:-veil-sec-harness:local}"
DF_BACKEND="veil-sec-diff-backend"
DF_NGINX="veil-sec-diff-nginx"
DF_VEIL="veil-sec-diff-veil"
REPORT="${RESULTS_DIR}/differential_report.txt"

mkdir -p "${RESULTS_DIR}"

if [[ "${SKIP_DIFFERENTIAL:-1}" == "1" ]]; then
    echo "differential: skipped (SKIP_DIFFERENTIAL=1)" | tee "${REPORT}"
    exit 0
fi

cleanup_df() {
    docker rm -f "${DF_BACKEND}" "${DF_NGINX}" "${DF_VEIL}" >/dev/null 2>&1 || true
    rm -f "${RESULTS_DIR}/diff-nginx.conf" "${RESULTS_DIR}/diff-veil.toml"
}
trap cleanup_df EXIT

: >"${REPORT}"
echo "differential start" | tee -a "${REPORT}"
docker network inspect "${NET_NAME}" >/dev/null 2>&1 || docker network create "${NET_NAME}" >/dev/null
cleanup_df

resolve_ip() { docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$1" 2>/dev/null || true; }

# 1) 共有バックエンド（200 を返す最小 echo）。harness イメージの Rust 製 bad-backend を
#    既定パスで使用すると、Content-Length のボディを読み切ってから 200 を返す。
docker run -d --name "${DF_BACKEND}" --network "${NET_NAME}" \
    --entrypoint /usr/local/bin/bad-backend \
    "${HARNESS_IMAGE}" >/dev/null

be_ip=""
for _ in $(seq 1 20); do be_ip="$(resolve_ip "${DF_BACKEND}")"; [[ -n "${be_ip}" ]] && break; sleep 0.5; done
[[ -n "${be_ip}" ]] || { echo "differential: backend IP 未解決" | tee -a "${REPORT}"; exit 0; }

# 2) nginx リバースプロキシ → backend
cat >"${RESULTS_DIR}/diff-nginx.conf" <<EOF
events {}
http {
    server {
        listen 80;
        location / { proxy_pass http://${be_ip}:8080; }
    }
}
EOF
docker run -d --name "${DF_NGINX}" --network "${NET_NAME}" \
    -v "${RESULTS_DIR}/diff-nginx.conf:/etc/nginx/nginx.conf:ro" \
    "${NGINX_IMAGE}" >/dev/null

# 3) Veil リバースプロキシ → backend（同一バックエンド）
cat >"${RESULTS_DIR}/diff-veil.toml" <<EOF
[server]
listen = "0.0.0.0:443"
http = "0.0.0.0:80"
threads = 0
[logging]
level = "warn"
[security]
allow_security_failures = false
enable_seccomp = true
enable_landlock = false
seccomp_mode = "filter"
[tls]
cert_path = "/etc/veil/ssl/cert.pem"
key_path = "/etc/veil/ssl/key.pem"
ktls_enabled = true
ktls_fallback_enabled = true
[upstreams."diff-pool"]
algorithm = "round_robin"
servers = ["http://${be_ip}:8080/"]
[[route]]
[route.conditions]
path = "/"
[route.action]
type = "Proxy"
upstream = "diff-pool"
[route.security]
allowed_methods = ["HEAD", "GET", "POST"]
EOF
docker run -d --name "${DF_VEIL}" --network "${NET_NAME}" --read-only \
    --tmpfs /var/cache/veil:rw,noexec,nosuid,uid=65532,gid=65532,size=128m \
    --tmpfs /var/tmp/veil:rw,noexec,nosuid,uid=65532,gid=65532,size=64m \
    -v "${RESULTS_DIR}/diff-veil.toml:/etc/veil/conf.d/config.toml:ro" \
    -v "${DOCKER_DIR}/assets/ssl:/etc/veil/ssl:ro" \
    --security-opt "seccomp=${SECCOMP}" \
    "${VEIL_IMAGE}" >/dev/null

nginx_ip=""; veil_ip=""
for _ in $(seq 1 20); do
    nginx_ip="$(resolve_ip "${DF_NGINX}")"; veil_ip="$(resolve_ip "${DF_VEIL}")"
    [[ -n "${nginx_ip}" && -n "${veil_ip}" ]] && break; sleep 0.5
done
sleep 4
[[ -n "${nginx_ip}" && -n "${veil_ip}" ]] || { echo "differential: proxy IP 未解決" | tee -a "${REPORT}"; exit 0; }

# 4) 差分プローブ: 同一リクエスト集合を両プロキシへ送りステータスを比較。
#    harness イメージ内の differential_probe.sh（bash /dev/tcp + openssl s_client）で実施。
#    既知の意図的差分（Veil はスマグリング要因を厳格拒否）は allowlist で扱う。
set +e
docker run --rm --network "${NET_NAME}" \
    --entrypoint /scripts/differential_probe.sh \
    "${HARNESS_IMAGE}" "${nginx_ip}" "${veil_ip}" 2>&1 | tee -a "${REPORT}"
rc="${PIPESTATUS[0]}"
set -e

if [[ "${rc}" -ne 0 ]]; then
    echo "differential: WARNING 予期しない差分あり（backlog 要確認）" | tee -a "${REPORT}"
    [[ "${DIFFERENTIAL_BLOCKING:-0}" == "1" ]] && exit 1
fi
echo "differential 完了: ${REPORT}"
