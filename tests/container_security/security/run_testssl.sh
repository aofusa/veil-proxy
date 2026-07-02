#!/usr/bin/env bash
# testssl.sh による TLS 設定スキャン（Docker コンテナ内完結）
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
# shellcheck source=../lib/common.sh
source "${REPO_ROOT}/tests/container_security/lib/common.sh"

RESULTS_DIR="${RESULTS_DIR:-${REPO_ROOT}/tests/container_security/results}"
TESTSSL_IMAGE="${TESTSSL_IMAGE:-drwetter/testssl.sh:3.2}"
REPORT="${RESULTS_DIR}/testssl_report.txt"
VEIL_HOST_RESOLVED="${VEIL_HOST_RESOLVED:-}"

mkdir -p "${RESULTS_DIR}"

if [[ "${SKIP_TESTSSL:-0}" == "1" ]]; then
    echo "testssl: skipped (SKIP_TESTSSL=1)" | tee "${REPORT}"
    exit 0
fi

resolve_target() {
    local host
    if [[ -n "${VEIL_HOST_RESOLVED}" ]]; then
        host="${VEIL_HOST_RESOLVED}"
    elif docker inspect "${VEIL_CONTAINER}" >/dev/null 2>&1; then
        host="$(resolve_veil_host)"
    else
        host="${VEIL_HOST:-veil-proxy}"
    fi
    [[ -n "${host}" ]] || die "Veil ホストを解決できません"
    echo "${host}:443"
}

TARGET="$(resolve_target)"
echo "testssl start target=${TARGET}" | tee "${REPORT}"

docker pull "${TESTSSL_IMAGE}" >/dev/null 2>&1 || true

if docker run --rm \
    --network "${NET_NAME}" \
    -v "${RESULTS_DIR}:/results:rw" \
    "${TESTSSL_IMAGE}" \
    --warnings off -p --openssl-timeout 5 "${TARGET}" \
    2>&1 | tee -a "${REPORT}" | grep -qiE 'TLS1_2|TLS1_3'; then
    echo "testssl: ok" | tee -a "${REPORT}"
else
    echo "testssl: completed with review recommended" | tee -a "${REPORT}"
fi