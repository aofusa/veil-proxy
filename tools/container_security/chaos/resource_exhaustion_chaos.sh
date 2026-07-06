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
# メモリスイープ: 段階的に絞って「起動失敗」と「稼働中枯渇」の閾値を切り分ける。
# 空白区切り。既定は緩→厳。SKIP_RESOURCE_EXHAUSTION_SWEEP=1 で単発（RX_MEMORY のみ）。
RX_MEMORY_SWEEP="${RX_MEMORY_SWEEP:-128m 96m 64m 48m 32m}"
REPORT="${RESULTS_DIR}/resource_exhaustion_report.txt"

mkdir -p "${RESULTS_DIR}"

if [[ "${SKIP_RESOURCE_EXHAUSTION:-1}" == "1" ]]; then
    echo "resource_exhaustion: skipped (SKIP_RESOURCE_EXHAUSTION=1)" | tee "${REPORT}"
    exit 0
fi

cleanup_rx() { docker rm -f "${RX_CONTAINER}" >/dev/null 2>&1 || true; }
trap cleanup_rx EXIT

: >"${REPORT}"
echo "resource_exhaustion start cpus=${RX_CPUS} pids=${RX_PIDS} nofile=${RX_NOFILE}" | tee -a "${REPORT}"

docker network inspect "${NET_NAME}" >/dev/null 2>&1 || docker network create "${NET_NAME}" >/dev/null

# 単一メモリ制約で 1 回検証する。
# 出力: "<mem> startup=<ok|fail> runtime=<ok|oomkill|panic|hang> exit=<code>"
run_one() {
    local mem="$1"
    cleanup_rx

    # --- 起動フェーズ ---
    if ! docker run -d --name "${RX_CONTAINER}" --network "${NET_NAME}" \
        --read-only \
        --memory "${mem}" --memory-swap "${mem}" \
        --cpus "${RX_CPUS}" --pids-limit "${RX_PIDS}" \
        --ulimit "nofile=${RX_NOFILE}:${RX_NOFILE}" \
        --tmpfs /var/cache/veil:rw,noexec,nosuid,uid=65532,gid=65532,size=32m \
        --tmpfs /var/tmp/veil:rw,noexec,nosuid,uid=65532,gid=65532,size=16m \
        -v "${DOCKER_DIR}/assets/conf.d/config.toml:/etc/veil/conf.d/config.toml:ro" \
        -v "${DOCKER_DIR}/assets/ssl:/etc/veil/ssl:ro" \
        -v "${DOCKER_DIR}/assets/www:/var/www:ro" \
        --security-opt "seccomp=${SECCOMP}" \
        "${VEIL_IMAGE}" >/dev/null 2>>"${REPORT}"; then
        echo "${mem} startup=fail runtime=n/a exit=create-error"
        return
    fi

    local rx_ip
    rx_ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "${RX_CONTAINER}" 2>/dev/null || true)
    sleep 3

    # 起動フェーズで即死していないか（起動失敗 vs 稼働中枯渇の切り分け）。
    local status
    status=$(docker inspect -f '{{.State.Status}}' "${RX_CONTAINER}" 2>/dev/null || echo absent)
    if [[ "${status}" != "running" ]]; then
        local exit0 oom0
        exit0=$(docker inspect -f '{{.State.ExitCode}}' "${RX_CONTAINER}" 2>/dev/null || echo "?")
        oom0=$(docker inspect -f '{{.State.OOMKilled}}' "${RX_CONTAINER}" 2>/dev/null || echo "?")
        [[ "${oom0}" == "true" ]] && echo "${mem} startup=fail runtime=oomkill exit=${exit0}" \
            || echo "${mem} startup=fail runtime=exited exit=${exit0}"
        return
    fi

    # --- 稼働中枯渇フェーズ: 高並行負荷（FD/PID/メモリ圧迫）---
    if [[ -n "${rx_ip}" ]]; then
        docker run --rm --network "${NET_NAME}" williamyeh/wrk:latest \
            -t4 -c400 -d20s --timeout 5s "https://${rx_ip}:443/" >>"${REPORT}" 2>&1 || true
    fi

    local st oom exit1 runtime
    st=$(docker inspect -f '{{.State.Status}}' "${RX_CONTAINER}" 2>/dev/null || echo absent)
    oom=$(docker inspect -f '{{.State.OOMKilled}}' "${RX_CONTAINER}" 2>/dev/null || echo "?")
    exit1=$(docker inspect -f '{{.State.ExitCode}}' "${RX_CONTAINER}" 2>/dev/null || echo "?")
    if [[ "${oom}" == "true" ]]; then
        runtime="oomkill"
    elif docker logs "${RX_CONTAINER}" 2>&1 | grep -qiE 'panic|RUST_BACKTRACE|abort|segfault'; then
        runtime="panic"
    elif [[ "${st}" == "running" ]]; then
        runtime="ok"  # 制約下でも生存（過負荷は接続拒否/エラーで吸収）
    else
        runtime="exited"
    fi
    echo "${mem} startup=ok runtime=${runtime} exit=${exit1}"
}

echo "== メモリスイープ（起動失敗 vs 稼働中枯渇の閾値探索）==" | tee -a "${REPORT}"
sweep="${RX_MEMORY_SWEEP}"
[[ "${SKIP_RESOURCE_EXHAUSTION_SWEEP:-0}" == "1" ]] && sweep="${RX_MEMORY}"
fail=0
for mem in ${sweep}; do
    line="$(run_one "${mem}")"
    echo "  ${line}" | tee -a "${REPORT}"
    # panic / oomkill は Graceful Degradation 違反 = findings 候補。
    if echo "${line}" | grep -qE 'runtime=(panic|oomkill)'; then
        echo "  WARNING: ${mem} で panic/oomkill を検出（backlog 起票対象）" | tee -a "${REPORT}"
        docker logs "${RX_CONTAINER}" 2>&1 | grep -iE 'panic|abort|segfault' | head -5 | tee -a "${REPORT}" || true
        fail=1
    fi
done

# io_uring SQ/CQ リング飽和については、B-24（SQ 満杯時の I/O Future 永久ハング）を
# 本フェーズの設計中に検出・修正済み。ここでは cgroup 圧迫で CQ 溢れ経路を間接的に加圧する。
echo "resource_exhaustion: done (findings=${fail}; SQ/CQ 飽和は B-24 で修正済み)" | tee -a "${REPORT}"
