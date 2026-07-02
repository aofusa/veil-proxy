#!/usr/bin/env bash
set -euo pipefail

PHASE="${PHASE:-all}"

run_phase() {
    case "$1" in
        health)
            exec /scripts/health_check.sh
            ;;
        fuzz)
            exec /scripts/fuzz_http.py
            ;;
        chaos)
            exec /scripts/chaos_load.sh
            ;;
        security)
            exec /scripts/security_scan.sh
            ;;
        all)
            /scripts/health_check.sh
            /scripts/fuzz_http.py
            /scripts/chaos_load.sh
            /scripts/security_scan.sh
            ;;
        *)
            echo "不明なフェーズ: $1" >&2
            exit 1
            ;;
    esac
}

if [[ $# -gt 0 ]]; then
    run_phase "$1"
else
    run_phase "${PHASE}"
fi