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

for probe in "${PROBES[@]}"; do
    echo "--- ${probe} ---" | tee -a "${REPORT}"
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