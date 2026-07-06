#!/usr/bin/env bash
# F-56: 負荷テストハーネス（wrk / k6）。docker のみで完結。
#
# 稼働中の Veil に対して高並行負荷をかけ、latency（p50/p90/p99）と error 率を計測する。
# `tools/container_security/` の chaos（toxiproxy 遅延・パケットロス等）と併用し、
# **chaos 前後の latency/error 率比較**を行うための土台（F-56 受け入れ条件）。
#
# 使い方:
#   TARGET_URL=https://127.0.0.1:443/ ./run_load.sh                 # wrk 単発
#   ENGINE=k6 TARGET_URL=https://127.0.0.1:443/ ./run_load.sh       # k6 で閾値判定
#   PHASE=chaos ./run_load.sh                                       # 出力ファイル名を分離
#
# 併用例（chaos 前後比較）:
#   PHASE=baseline ./run_load.sh
#   （tools/container_security の chaos を注入）
#   PHASE=chaos    ./run_load.sh
#   ./compare.sh                                                    # baseline vs chaos の差分
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RESULTS_DIR="${RESULTS_DIR:-${SCRIPT_DIR}/results}"
TARGET_URL="${TARGET_URL:-https://127.0.0.1:443/}"
ENGINE="${ENGINE:-wrk}"           # wrk | k6
PHASE="${PHASE:-baseline}"        # baseline | chaos（出力ファイル名の接尾辞）
DURATION="${DURATION:-20s}"
CONNECTIONS="${CONNECTIONS:-200}"
THREADS="${THREADS:-4}"
# k6 の合否閾値（error 率 / p95 latency ミリ秒）。
K6_MAX_ERROR_RATE="${K6_MAX_ERROR_RATE:-0.05}"
K6_MAX_P95_MS="${K6_MAX_P95_MS:-1000}"
WRK_IMAGE="${WRK_IMAGE:-williamyeh/wrk:latest}"
K6_IMAGE="${K6_IMAGE:-grafana/k6:latest}"

mkdir -p "${RESULTS_DIR}"
report="${RESULTS_DIR}/load_${ENGINE}_${PHASE}.txt"

echo "load(${ENGINE}) phase=${PHASE} target=${TARGET_URL} c=${CONNECTIONS} d=${DURATION}" | tee "${report}"

case "${ENGINE}" in
    wrk)
        # --latency で分布（p50/p90/p99）を出力。--timeout で遅延注入下のハングを防ぐ。
        docker run --rm --network host "${WRK_IMAGE}" \
            -t"${THREADS}" -c"${CONNECTIONS}" -d"${DURATION}" --latency --timeout 5s \
            "${TARGET_URL}" 2>&1 | tee -a "${report}"
        ;;
    k6)
        # k6 スクリプトを stdin で渡し、閾値（error 率・p95 latency）で合否判定する。
        docker run --rm --network host -i \
            -e TARGET_URL="${TARGET_URL}" \
            -e DURATION="${DURATION}" \
            -e CONNECTIONS="${CONNECTIONS}" \
            -e MAX_ERROR_RATE="${K6_MAX_ERROR_RATE}" \
            -e MAX_P95_MS="${K6_MAX_P95_MS}" \
            "${K6_IMAGE}" run - <"${SCRIPT_DIR}/k6_load.js" 2>&1 | tee -a "${report}"
        ;;
    *)
        echo "unknown ENGINE='${ENGINE}' (expected wrk|k6)" | tee -a "${report}"
        exit 2
        ;;
esac

echo "load: report -> ${report}"
