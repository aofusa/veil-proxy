#!/usr/bin/env bash
# F-76: HTTP リクエストスマグリング能動テスト（CL.TE / TE.CL / 複数 CL）。
# ZAP baseline（受動）ではカバーされないプロキシ特有の高リスク領域を能動検査する。
# 稼働中の Veil コンテナへ crafted な生リクエストを TLS 経由で送り、曖昧フレーミングが
# 一貫して 400 で拒否される（＝バックエンドへ転送されずスマグリング不成立）ことを確認。
# docker のみで完結（python:3-slim + 標準ライブラリ ssl/socket）。
#
# 前提: run.sh から呼ばれ、Veil コンテナが ${NET_NAME} 上で稼働していること。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
RESULTS_DIR="${RESULTS_DIR:-${REPO_ROOT}/tools/container_security/results}"
HARNESS_IMAGE="${HARNESS_IMAGE:-veil-sec-harness:local}"
NET_NAME="${NET_NAME:-veil-sec-test-net}"
VEIL_CONTAINER="${VEIL_CONTAINER:-veil-sec-proxy}"
REPORT="${RESULTS_DIR}/smuggling_report.txt"

mkdir -p "${RESULTS_DIR}"

if [[ "${SKIP_SMUGGLING:-0}" == "1" ]]; then
    echo "smuggling: skipped (SKIP_SMUGGLING=1)" | tee "${REPORT}"
    exit 0
fi

ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' \
    "${VEIL_CONTAINER}" 2>/dev/null || true)
if [[ -z "${ip}" ]]; then
    echo "smuggling: skipped (Veil コンテナ未起動)" | tee "${REPORT}"
    exit 0
fi

: >"${REPORT}"
echo "smuggling active test start target=${ip}:443" | tee -a "${REPORT}"

# プローブは harness イメージ内の smuggling_probe.sh（bash + openssl s_client）で実施する。
# 各ベクタを生バイト列で TLS 送信し、ステータス行を検査する。
# 期待: CL.TE / TE.CL / 複数 CL は 400。単独 chunked は非 400（誤検知しないこと）。
set +e
docker run --rm --network "${NET_NAME}" \
    --entrypoint /scripts/smuggling_probe.sh \
    "${HARNESS_IMAGE}" "${ip}" 2>&1 | tee -a "${REPORT}"
rc="${PIPESTATUS[0]}"
set -e

# 非ブロッキング（検出＝実装の退行なので警告としてレポートに残すが suite は継続）。
if [[ "${rc}" -ne 0 ]]; then
    echo "smuggling: WARNING 一部ベクタが期待通り拒否されず（退行の可能性・backlog 要確認）" | tee -a "${REPORT}"
    [[ "${SMUGGLING_BLOCKING:-0}" == "1" ]] && exit 1
fi

echo "smuggling 完了: ${REPORT}"
