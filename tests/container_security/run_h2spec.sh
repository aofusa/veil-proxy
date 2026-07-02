#!/usr/bin/env bash
# h2spec フェーズのみ実行（開発・デバッグ用）
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib/common.sh
source "${SCRIPT_DIR}/lib/common.sh"

main() {
    trap cleanup EXIT
    require_docker
    mkdir -p "${RESULTS_DIR}"
    cleanup || true

    build_harness_image
    start_network
    start_veil_container
    wait_veil_healthy 90
    run_harness h2spec h2spec
    run_harness health health

    log "h2spec 完了: ${RESULTS_DIR}/h2spec_report.txt"
}

main "$@"