#!/usr/bin/env bash
# SBOM（ソフトウェア部品表）生成。将来のゼロデイ脆弱性の影響調査を迅速化する。
# syft でソースツリー（Cargo.lock）とコンテナイメージの双方から CycloneDX / SPDX を出力。
# docker のみで完結（anchore/syft イメージ）。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
RESULTS_DIR="${RESULTS_DIR:-${REPO_ROOT}/tools/container_security/results}"
SYFT_IMAGE="${SYFT_IMAGE:-anchore/syft:latest}"
VEIL_IMAGE="${VEIL_IMAGE:-veil:glibc}"
REPORT="${RESULTS_DIR}/sbom_report.txt"

mkdir -p "${RESULTS_DIR}"

if [[ "${SKIP_SBOM:-0}" == "1" ]]; then
    echo "sbom: skipped (SKIP_SBOM=1)" | tee "${REPORT}"
    exit 0
fi

echo "sbom start image=${VEIL_IMAGE}" | tee "${REPORT}"

# 1) ソースツリー（Cargo.lock）から CycloneDX JSON
docker run --rm \
    -v "${REPO_ROOT}:/src:ro" \
    -v "${RESULTS_DIR}:/results:rw" \
    "${SYFT_IMAGE}" \
    scan dir:/src --source-name veil-src \
    --exclude './target/**' --exclude './fuzz/target/**' --exclude './.git/**' \
    -o cyclonedx-json=/results/sbom_source.cdx.json 2>&1 | tee -a "${REPORT}" || true

# 2) コンテナイメージ（docker デーモン経由）から SPDX JSON
docker run --rm \
    -v /var/run/docker.sock:/var/run/docker.sock:ro \
    -v "${RESULTS_DIR}:/results:rw" \
    "${SYFT_IMAGE}" \
    scan "docker:${VEIL_IMAGE}" --source-name "${VEIL_IMAGE}" \
    -o spdx-json=/results/sbom_image.spdx.json 2>&1 | tee -a "${REPORT}" || true

for f in sbom_source.cdx.json sbom_image.spdx.json; do
    [[ -s "${RESULTS_DIR}/${f}" ]] && echo "sbom generated: ${f}" | tee -a "${REPORT}"
done
echo "sbom: ok" | tee -a "${REPORT}"
