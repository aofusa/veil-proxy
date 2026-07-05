#!/usr/bin/env bash
# F-75: シークレットスキャン（gitleaks）。鍵・トークンの誤コミットを検出する。
# docker のみで完結。既定は非ブロッキング（検出しても suite は継続、レポートで報告）。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
RESULTS_DIR="${RESULTS_DIR:-${REPO_ROOT}/tools/container_security/results}"
GITLEAKS_IMAGE="${GITLEAKS_IMAGE:-zricethezav/gitleaks:latest}"
REPORT="${RESULTS_DIR}/gitleaks_report.txt"
SARIF="${RESULTS_DIR}/gitleaks_report.sarif"

mkdir -p "${RESULTS_DIR}"

if [[ "${SKIP_GITLEAKS:-0}" == "1" ]]; then
    echo "gitleaks: skipped (SKIP_GITLEAKS=1)" | tee "${REPORT}"
    exit 0
fi

echo "gitleaks start" | tee "${REPORT}"

# `detect` は git 履歴全体を走査する。CI では全履歴、ローカルでは `--no-git`
# でワークツリーのみを走査する切替を GITLEAKS_MODE で提供する。
mode="${GITLEAKS_MODE:-dir}"
extra_args=()
if [[ "${mode}" == "dir" ]]; then
    # ワークツリー（git 履歴なし）を走査。.git を持たない配布物でも動く。
    extra_args+=(--no-git)
fi

# gitleaks の exit code: 0=検出なし, 1=検出あり, 2=エラー。
# 非ブロッキング運用のため 1（検出）は警告としてレポートに残し suite を継続する。
set +e
docker run --rm \
    -v "${REPO_ROOT}:/repo:ro" \
    -v "${RESULTS_DIR}:/results:rw" \
    "${GITLEAKS_IMAGE}" \
    detect --source=/repo \
    "${extra_args[@]}" \
    --report-format sarif \
    --report-path /results/gitleaks_report.sarif \
    --redact \
    --verbose \
    2>&1 | tee -a "${REPORT}"
rc="${PIPESTATUS[0]}"
set -e

case "${rc}" in
    0)
        echo "gitleaks: ok (no leaks)" | tee -a "${REPORT}"
        ;;
    1)
        # 検出あり。SARIF から件数を抽出してレポートへ要約（誤検知トリアージは backlog）。
        count="$(grep -c '"ruleId"' "${SARIF}" 2>/dev/null || echo '?')"
        echo "gitleaks: findings detected (count=${count}) — triage required, see ${SARIF}" \
            | tee -a "${REPORT}"
        # 非ブロッキング: suite を止めない（exit 0）。ブロッキングにしたい場合は
        # GITLEAKS_BLOCKING=1 で失敗させる。
        if [[ "${GITLEAKS_BLOCKING:-0}" == "1" ]]; then
            exit 1
        fi
        ;;
    *)
        echo "gitleaks: error (rc=${rc})" | tee -a "${REPORT}"
        # 実行エラーは suite をブロックしない（ツール障害を機能失敗と区別）。
        ;;
esac

echo "gitleaks 完了: ${REPORT}"
