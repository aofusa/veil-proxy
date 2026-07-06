#!/usr/bin/env bash
# F-56: baseline vs chaos の負荷計測差分レポート。
#
# run_load.sh を PHASE=baseline / PHASE=chaos で実行して得た 2 つの wrk レポートから、
# Requests/sec・latency 分布・Non-2xx を並べて劣化幅を可視化する。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RESULTS_DIR="${RESULTS_DIR:-${SCRIPT_DIR}/results}"
ENGINE="${ENGINE:-wrk}"
base="${RESULTS_DIR}/load_${ENGINE}_baseline.txt"
chaos="${RESULTS_DIR}/load_${ENGINE}_chaos.txt"
out="${RESULTS_DIR}/load_compare_${ENGINE}.txt"

for f in "${base}" "${chaos}"; do
    [[ -f "${f}" ]] || { echo "missing ${f}（先に run_load.sh を baseline/chaos で実行）"; exit 1; }
done

# wrk 出力から主要指標を抜き出す。
extract() {
    grep -E 'Requests/sec|Latency|Non-2xx|Socket errors|Transfer/sec' "$1" || true
}

{
    echo "== F-56 load comparison (${ENGINE}) =="
    echo "--- baseline (${base}) ---"
    extract "${base}"
    echo "--- chaos (${chaos}) ---"
    extract "${chaos}"
} | tee "${out}"

echo "compare: report -> ${out}"
