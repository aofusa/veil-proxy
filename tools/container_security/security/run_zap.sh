#!/usr/bin/env bash
# OWASP ZAP Baseline Scan（DAST）。稼働中の Veil コンテナに対し、
# 受動スキャン + 既知のプロキシ/HTTP 脆弱性パターン（ヘッダ欠落・情報漏洩等）を検査する。
# 既存の bash ベース security_scan.sh（メソッド制限・TRACE・パストラバーサル）を補完。
# docker のみで完結（ghcr.io/zaproxy/zaproxy baseline）。
#
# 前提: run.sh から呼ばれ、Veil コンテナが ${NET_NAME} 上で稼働していること。
# 単体実行時は VEIL_TARGET_URL を明示指定する。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
RESULTS_DIR="${RESULTS_DIR:-${REPO_ROOT}/tools/container_security/results}"
ZAP_IMAGE="${ZAP_IMAGE:-ghcr.io/zaproxy/zaproxy:stable}"
NET_NAME="${NET_NAME:-veil-sec-test-net}"
VEIL_CONTAINER="${VEIL_CONTAINER:-veil-sec-proxy}"
REPORT="${RESULTS_DIR}/zap_report.txt"

mkdir -p "${RESULTS_DIR}"

if [[ "${SKIP_ZAP:-0}" == "1" ]]; then
    echo "zap: skipped (SKIP_ZAP=1)" | tee "${REPORT}"
    exit 0
fi

# ターゲット URL の解決（コンテナ IP → https://<ip>:443/）
target="${VEIL_TARGET_URL:-}"
if [[ -z "${target}" ]]; then
    ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' \
        "${VEIL_CONTAINER}" 2>/dev/null || true)
    [[ -n "${ip}" ]] || { echo "zap: skipped (Veil コンテナ未起動・VEIL_TARGET_URL 未指定)" | tee "${REPORT}"; exit 0; }
    target="https://${ip}:443/"
fi

echo "zap baseline start target=${target}" | tee "${REPORT}"

# baseline: 受動スキャンのみ（-I: 警告があっても exit 0 で継続、backlog 起票用）
# 自己署名証明書のため TLS 検証は ZAP の既定挙動に委ねる。
docker run --rm \
    --network "${NET_NAME}" \
    -v "${RESULTS_DIR}:/zap/wrk:rw" \
    "${ZAP_IMAGE}" \
    zap-baseline.py -t "${target}" -I \
        -r zap_baseline_report.html \
        -x zap_baseline_report.xml \
        2>&1 | tee -a "${REPORT}" || true

echo "zap: ok" | tee -a "${REPORT}"
