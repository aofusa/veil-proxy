#!/usr/bin/env bash
# F-90: full features セキュリティプローブ群のオーケストレーション
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
RESULTS_DIR="${RESULTS_DIR:-${REPO_ROOT}/tools/container_security/results}"
HARNESS_IMAGE="${HARNESS_IMAGE:-veil-sec-harness:local}"
NET_NAME="${NET_NAME:-veil-sec-test-net}"
VEIL_CONTAINER="${VEIL_CONTAINER:-veil-sec-proxy}"
REPORT="${RESULTS_DIR}/full_features_security_report.txt"

mkdir -p "${RESULTS_DIR}"

if [[ "${SKIP_FULL_FEATURES_SECURITY:-0}" == "1" ]]; then
    echo "full_features_security: skipped (SKIP_FULL_FEATURES_SECURITY=1)" | tee "${REPORT}"
    exit 0
fi

ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' \
    "${VEIL_CONTAINER}" 2>/dev/null || true)
if [[ -z "${ip}" ]]; then
    echo "full_features_security: skipped (Veil コンテナ未起動)" | tee "${REPORT}"
    exit 0
fi

: >"${REPORT}"
echo "full_features_security start target=${ip}" | tee -a "${REPORT}"

PROBES=(
    admin_security_probe
    compression_cache_probe
    access_log_probe
    http3_probe
    h3_flood_probe
    l4_flood_probe
    websocket_probe
    grpc_probe
    grpc_web_probe
    rate_limit_probe
    wasm_security_probe
    metrics_probe
    otel_probe
)

# W-04: 不正 WASM モジュール（ホスト docker から ephemeral 起動）
echo "--- wasm_invalid_module_probe ---" | tee -a "${REPORT}"
set +e
"${SCRIPT_DIR}/../harness/scripts/wasm_invalid_module_probe.sh" 2>&1 | tee -a "${REPORT}"
host_rc="${PIPESTATUS[0]}"
set -e
total_fails=0
if [[ "${host_rc}" -ne 0 ]]; then
    total_fails=$((total_fails + 1))
    echo "wasm_invalid_module_probe: FAIL (rc=${host_rc})" | tee -a "${REPORT}"
else
    echo "wasm_invalid_module_probe: ok" | tee -a "${REPORT}"
fi

# veil コンテナのメモリ使用量（MiB。docker stats の "123.4MiB / 7.6GiB" の左側）
mem_mib() {
    docker stats --no-stream --format '{{.MemUsage}}' "${VEIL_CONTAINER}" 2>/dev/null | awk '{
        v = $1; u = v; gsub(/[0-9.]/, "", u); gsub(/[^0-9.]/, "", v)
        if (u == "GiB") v *= 1024; else if (u == "KiB" || u == "kB") v /= 1024; else if (u == "B") v /= 1048576
        printf "%d", v }'
}
# F-279（B-222）: 偽装 Initial の洪水で増えてよいメモリの上限（MiB）
H3_FLOOD_MAX_GROWTH_MB="${H3_FLOOD_MAX_GROWTH_MB:-256}"

for probe in "${PROBES[@]}"; do
    echo "--- ${probe} ---" | tee -a "${REPORT}"
    mem_before=""
    [[ "${probe}" == "h3_flood_probe" ]] && mem_before="$(mem_mib)"
    set +e
    docker run --rm --network "${NET_NAME}" \
        -e "VEIL_HOST=veil-proxy" \
        -e "VEIL_SNI=veil-proxy" \
        -e "ADMIN_SECRET=${ADMIN_SECRET:-veil-sec-test-admin}" \
        -e "METRICS_PATH=${METRICS_PATH:-/__metrics}" \
        -v "${RESULTS_DIR}:/results:rw" \
        --entrypoint "/scripts/${probe}.sh" \
        "${HARNESS_IMAGE}" 2>&1 | tee -a "${REPORT}"
    rc="${PIPESTATUS[0]}"
    set -e
    if [[ -n "${mem_before}" ]]; then
        mem_after="$(mem_mib)"
        growth=$(( ${mem_after:-0} - ${mem_before:-0} ))
        echo "h3_flood memory: before=${mem_before}MiB after=${mem_after}MiB growth=${growth}MiB (limit ${H3_FLOOD_MAX_GROWTH_MB}MiB)" | tee -a "${REPORT}"
        if [[ -z "${mem_after}" || "${growth}" -gt "${H3_FLOOD_MAX_GROWTH_MB}" ]]; then
            rc=1
        fi
    fi
    if [[ "${rc}" -ne 0 ]]; then
        total_fails=$((total_fails + 1))
        echo "${probe}: FAIL (rc=${rc})" | tee -a "${REPORT}"
    else
        echo "${probe}: ok" | tee -a "${REPORT}"
    fi
done

if [[ "${total_fails}" -eq 0 ]]; then
    echo "full_features_security: ok" | tee -a "${REPORT}"
    exit 0
fi

echo "full_features_security: FAILURES (probes_failed=${total_fails})" | tee -a "${REPORT}"
[[ "${FULL_FEATURES_SECURITY_BLOCKING:-0}" == "1" ]] && exit 1
exit 0