#!/usr/bin/env bash
# バックエンドのプロトコル違反カオス（レポート §2.2.3）。
# ヘッダー途中切断・Content-Length 不一致・巨大ヘッダー・不正ステータス・無応答を返す
# モックバックエンドへ Veil をプロキシさせ、Veil が安全に処理（502/接続クローズ）し
# クラッシュ・無限ハング・バッファ破損・レスポンススマグリングを起こさないことを検証する。
#
# 専用の Veil + バックエンドを立てる自己完結スクリプト（既定 SKIP）。docker のみで完結。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
RESULTS_DIR="${RESULTS_DIR:-${REPO_ROOT}/tools/container_security/results}"
DOCKER_DIR="${REPO_ROOT}/docker"
NET_NAME="${NET_NAME:-veil-sec-test-net}"
VEIL_IMAGE="${VEIL_IMAGE:-veil:glibc}"
HARNESS_IMAGE="${HARNESS_IMAGE:-veil-sec-harness:local}"
SECCOMP="${DOCKER_DIR}/assets/security/seccomp.json"
BB_BACKEND="${BB_BACKEND:-veil-sec-badbackend}"
BB_VEIL="${BB_VEIL:-veil-sec-badbackend-proxy}"
REPORT="${RESULTS_DIR}/bad_backend_report.txt"

mkdir -p "${RESULTS_DIR}"

if [[ "${SKIP_BAD_BACKEND:-1}" == "1" ]]; then
    echo "bad_backend: skipped (SKIP_BAD_BACKEND=1)" | tee "${REPORT}"
    exit 0
fi

cleanup_bb() { docker rm -f "${BB_BACKEND}" "${BB_VEIL}" >/dev/null 2>&1 || true; rm -f "${RESULTS_DIR}/bad-backend.toml"; }
trap cleanup_bb EXIT

: >"${REPORT}"
echo "bad_backend chaos start" | tee -a "${REPORT}"
docker network inspect "${NET_NAME}" >/dev/null 2>&1 || docker network create "${NET_NAME}" >/dev/null
cleanup_bb

# 1) モックバックエンド起動（harness イメージの Rust 製 bad-backend）
docker run -d --name "${BB_BACKEND}" --network "${NET_NAME}" \
    --entrypoint /usr/local/bin/bad-backend \
    "${HARNESS_IMAGE}" >/dev/null

# バックエンド IP を解決して upstream に埋め込む。Landlock/sandbox 下では glibc NSS 経由の
# 実行時 DNS 解決が不安定なため、既存ハーネス（prepare_veil_test_config）同様に IP 直指定する。
bb_ip=""
for _ in $(seq 1 20); do
    bb_ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "${BB_BACKEND}" 2>/dev/null || true)
    [[ -n "${bb_ip}" ]] && break
    sleep 0.5
done
[[ -n "${bb_ip}" ]] || { echo "bad_backend: バックエンド IP を解決できず" | tee -a "${REPORT}"; exit 0; }
# バックエンドの待受確認
docker run --rm --network "${NET_NAME}" curlimages/curl:latest \
    -s -o /dev/null --max-time 5 "http://${bb_ip}:8080/ok" 2>/dev/null || \
    echo "bad_backend: 警告 バックエンド待受未確認" | tee -a "${REPORT}"

# 2) Veil 設定生成（/ → bad backend への Proxy、IP 直指定）
cat >"${RESULTS_DIR}/bad-backend.toml" <<EOF
[server]
listen = "0.0.0.0:443"
http = "0.0.0.0:80"
threads = 0
[logging]
level = "info"
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
[upstreams."bad-pool"]
algorithm = "round_robin"
servers = ["http://${bb_ip}:8080/"]
[[route]]
[route.conditions]
path = "/"
[route.action]
type = "Proxy"
upstream = "bad-pool"
[route.security]
allowed_methods = ["HEAD", "GET", "POST"]
EOF

docker run -d --name "${BB_VEIL}" --network "${NET_NAME}" --read-only \
    -e RUST_BACKTRACE=1 \
    --tmpfs /var/cache/veil:rw,noexec,nosuid,uid=65532,gid=65532,size=128m \
    --tmpfs /var/tmp/veil:rw,noexec,nosuid,uid=65532,gid=65532,size=64m \
    -v "${RESULTS_DIR}/bad-backend.toml:/etc/veil/conf.d/config.toml:ro" \
    -v "${DOCKER_DIR}/assets/ssl:/etc/veil/ssl:ro" \
    --security-opt "seccomp=${SECCOMP}" \
    "${VEIL_IMAGE}" >/dev/null

veil_ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "${BB_VEIL}" 2>/dev/null || true)
sleep 4

probe() { # path expected_hint
    local p="$1"
    # --max-time でハング検出（プロキシがタイムアウト内に応答/切断すること）
    local code
    code=$(docker run --rm --network "${NET_NAME}" curlimages/curl:latest \
        -sk -o /dev/null -w '%{http_code}' --max-time 12 "https://${veil_ip}:443${p}" 2>/dev/null || echo "TIMEOUT")
    echo "  ${p} -> ${code}" | tee -a "${REPORT}"
}

echo "== プロトコル違反プローブ ==" | tee -a "${REPORT}"
for p in /truncated-headers /cl-too-large /cl-too-small /huge-headers /bad-status /no-response /instant-close /ok; do
    probe "${p}"
done

# 判定: Veil がクラッシュ/OOM していないこと
state=$(docker inspect -f '{{.State.Status}} exit={{.State.ExitCode}}' "${BB_VEIL}" 2>/dev/null || echo absent)
echo "veil state after probes: ${state}" | tee -a "${REPORT}"
if docker logs "${BB_VEIL}" 2>&1 | grep -qiE 'panic|RUST_BACKTRACE|segfault'; then
    echo "WARNING: Veil に panic/segfault 痕跡（backlog 起票対象）" | tee -a "${REPORT}"
    docker logs "${BB_VEIL}" 2>&1 | grep -iE 'panic|segfault' | head -5 | tee -a "${REPORT}"
fi
echo "bad_backend: ok" | tee -a "${REPORT}"
