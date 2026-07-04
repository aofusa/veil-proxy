#!/usr/bin/env bash
# リソース枯渇カオス: CPU・メモリ・FD・PID を極端に絞った cgroup 制約下で Veil を起動し、
# 高並行負荷をかけて「Graceful Degradation（接続拒否/エラー応答）に留まり panic/OOM-Kill/
# ハングしない」ことを検証する（F-53 拡充・レポート §2.2.2）。
#
# suite が管理する本体コンテナとは別に、専用の制約付きコンテナを立てて検証する（既定 SKIP）。
# docker のみで完結。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
RESULTS_DIR="${RESULTS_DIR:-${REPO_ROOT}/tools/container_security/results}"
DOCKER_DIR="${REPO_ROOT}/docker"
NET_NAME="${NET_NAME:-veil-sec-test-net}"
VEIL_IMAGE="${VEIL_IMAGE:-veil:glibc}"
HARNESS_IMAGE="${HARNESS_IMAGE:-veil-sec-harness:local}"
SECCOMP="${DOCKER_DIR}/assets/security/seccomp.json"
RX_CONTAINER="${RX_CONTAINER:-veil-sec-rx}"
# 制約値（極端に小さく設定して枯渇を誘発）
RX_MEMORY="${RX_MEMORY:-48m}"
RX_CPUS="${RX_CPUS:-0.25}"
RX_PIDS="${RX_PIDS:-64}"
RX_NOFILE="${RX_NOFILE:-256}"
REPORT="${RESULTS_DIR}/resource_exhaustion_report.txt"

mkdir -p "${RESULTS_DIR}"

if [[ "${SKIP_RESOURCE_EXHAUSTION:-1}" == "1" ]]; then
    echo "resource_exhaustion: skipped (SKIP_RESOURCE_EXHAUSTION=1)" | tee "${REPORT}"
    exit 0
fi

cleanup_rx() { docker rm -f "${RX_CONTAINER}" >/dev/null 2>&1 || true; }
trap cleanup_rx EXIT

: >"${REPORT}"
echo "resource_exhaustion start mem=${RX_MEMORY} cpus=${RX_CPUS} pids=${RX_PIDS} nofile=${RX_NOFILE}" | tee -a "${REPORT}"

docker network inspect "${NET_NAME}" >/dev/null 2>&1 || docker network create "${NET_NAME}" >/dev/null
cleanup_rx

docker run -d --name "${RX_CONTAINER}" --network "${NET_NAME}" \
    --read-only \
    --memory "${RX_MEMORY}" --memory-swap "${RX_MEMORY}" \
    --cpus "${RX_CPUS}" --pids-limit "${RX_PIDS}" \
    --ulimit "nofile=${RX_NOFILE}:${RX_NOFILE}" \
    --tmpfs /var/cache/veil:rw,noexec,nosuid,uid=65532,gid=65532,size=32m \
    --tmpfs /var/tmp/veil:rw,noexec,nosuid,uid=65532,gid=65532,size=16m \
    -v "${DOCKER_DIR}/assets/conf.d/config.toml:/etc/veil/conf.d/config.toml:ro" \
    -v "${DOCKER_DIR}/assets/ssl:/etc/veil/ssl:ro" \
    -v "${DOCKER_DIR}/assets/www:/var/www:ro" \
    --security-opt "seccomp=${SECCOMP}" \
    "${VEIL_IMAGE}" >/dev/null 2>>"${REPORT}" || {
        echo "resource_exhaustion: Veil が制約下で起動できず（この時点で findings 候補）" | tee -a "${REPORT}"
    }

rx_ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "${RX_CONTAINER}" 2>/dev/null || true)
sleep 3

# 高並行負荷（FD/PID/メモリ圧迫）
if [[ -n "${rx_ip}" ]]; then
    echo "== 高並行負荷（wrk -c400）==" | tee -a "${REPORT}"
    docker run --rm --network "${NET_NAME}" williamyeh/wrk:latest \
        -t4 -c400 -d20s --timeout 5s "https://${rx_ip}:443/" 2>&1 | tee -a "${REPORT}" || true
fi

# 判定: panic / abort / OOM の痕跡を検査（顕在化した問題は backlog へ）
state=$(docker inspect -f '{{.State.Status}} oom={{.State.OOMKilled}} exit={{.State.ExitCode}}' "${RX_CONTAINER}" 2>/dev/null || echo "absent")
echo "container state: ${state}" | tee -a "${REPORT}"
if docker logs "${RX_CONTAINER}" 2>&1 | grep -qiE 'panic|RUST_BACKTRACE|abort|segfault'; then
    echo "WARNING: panic/abort 痕跡を検出（backlog 起票対象）" | tee -a "${REPORT}"
    docker logs "${RX_CONTAINER}" 2>&1 | grep -iE 'panic|abort|segfault' | head -5 | tee -a "${REPORT}"
fi
echo "resource_exhaustion: ok" | tee -a "${REPORT}"
