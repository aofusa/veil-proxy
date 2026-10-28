#!/usr/bin/env bash
# semgrep による SAST（自作コード＝特に src/runtime/・unsafe 周辺の静的解析）
# cargo-audit は依存関係のみを見るため、コードレベルの脆弱パターンを補完する。
# docker のみで完結（semgrep/semgrep イメージ、Rust ルールセット + カスタムルール）。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
RESULTS_DIR="${RESULTS_DIR:-${REPO_ROOT}/tools/container_security/results}"
SEMGREP_IMAGE="${SEMGREP_IMAGE:-semgrep/semgrep:latest}"
# p/rust: コミュニティ Rust ルール。p/security-audit: 一般的な危険パターン。
SEMGREP_CONFIG="${SEMGREP_CONFIG:-p/rust p/security-audit}"
REPORT="${RESULTS_DIR}/semgrep_report.txt"
SARIF="${RESULTS_DIR}/semgrep.sarif"

mkdir -p "${RESULTS_DIR}"

if [[ "${SKIP_SEMGREP:-0}" == "1" ]]; then
    echo "semgrep: skipped (SKIP_SEMGREP=1)" | tee "${REPORT}"
    exit 0
fi

echo "semgrep start config=${SEMGREP_CONFIG}" | tee "${REPORT}"

# --error は付けない（findings があっても suite は継続、結果は backlog 起票用）
config_args=()
for c in ${SEMGREP_CONFIG}; do config_args+=(--config "${c}"); done
# F-64: Veil 固有のカスタムルール（.semgrep/）。io_uring/ゼロコピー/禁止事項の
# 意味のある差分だけを拾う。SEMGREP_CUSTOM_RULES=0 で無効化可能。
if [[ "${SEMGREP_CUSTOM_RULES:-1}" == "1" && -d "${REPO_ROOT}/.semgrep" ]]; then
    config_args+=(--config /repo/.semgrep)
fi

# B-243: リポジトリは /repo に置き、作業ディレクトリから `.` を走査する（/src に置くと全ファイルのパスに "src/" が
# 含まれ、ルールの paths.include "src/" が third_party・examples にも一致していた）
docker run --rm \
    -v "${REPO_ROOT}:/repo:ro" \
    -v "${RESULTS_DIR}:/results:rw" \
    -w /repo \
    "${SEMGREP_IMAGE}" \
    semgrep scan "${config_args[@]}" \
        --metrics off \
        --sarif-output=/results/semgrep.sarif \
        --exclude target --exclude fuzz/target \
        . 2>&1 | tee -a "${REPORT}" || true

# findings 件数を要約（SARIF から results 配列長を素朴に数える）
if [[ -f "${SARIF}" ]]; then
    n=$(grep -o '"ruleId"' "${SARIF}" | wc -l | tr -d ' ')
    echo "semgrep findings=${n} (詳細: ${SARIF})" | tee -a "${REPORT}"
fi
echo "semgrep: ok" | tee -a "${REPORT}"
