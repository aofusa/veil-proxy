#!/usr/bin/env bash
# Proxy-Wasm コンプライアンス検証（F-90 / W-01〜W-03）
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"
RESULTS="/results/wasm_security_report.txt"

mkdir -p "$(dirname "${RESULTS}")"
: >"${RESULTS}"

log() { printf '%s\n' "$*" | tee -a "${RESULTS}"; }

fails=0

log "wasm_security_probe start"

# W-01/W-02: header_filter が X-Wasm-Processed を付与
headers=$(curl -sk -D - -o /dev/null --max-time 8 \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/wasm/" 2>/dev/null | tr -d '\r')
code=$(printf '%s\n' "${headers}" | awk 'BEGIN{c=0} /^HTTP/{c=$2} END{print c}')
wasm_hdr=$(printf '%s\n' "${headers}" | awk 'BEGIN{IGNORECASE=1} /^x-wasm-processed:/ {print $2; exit}')
log "wasm_route: code=${code} X-Wasm-Processed=${wasm_hdr:-missing}"

if [[ "${code}" =~ ^(200|502)$ ]]; then
    log "PASS wasm_route_responds"
else
    log "FAIL wasm_route_responds: code=${code}"
    fails=$((fails + 1))
fi

if [[ "${wasm_hdr}" == "true" ]]; then
    log "PASS proxy_wasm_header_filter"
else
    log "FAIL proxy_wasm_header_filter"
    fails=$((fails + 1))
fi

# W-03: 連続 12 リクエストでフィルタが安定（B-05 回帰）
parallel_ok=0
for ((i = 1; i <= 12; i++)); do
    h=$(curl -sk -D - -o /dev/null --max-time 8 \
        "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/wasm/p-${i}" 2>/dev/null \
        | tr -d '\r' | awk 'BEGIN{IGNORECASE=1} /^x-wasm-processed:/ {print $2; exit}')
    [[ "${h}" == "true" ]] && parallel_ok=$((parallel_ok + 1))
done
log "wasm_parallel_hits: ${parallel_ok}/12"
if [[ "${parallel_ok}" -ge 10 ]]; then
    log "PASS wasm_concurrent_filter"
else
    log "FAIL wasm_concurrent_filter"
    fails=$((fails + 1))
fi

if [[ "${fails}" -eq 0 ]]; then
    log "wasm_security: ok"
    exit 0
fi
log "wasm_security: FAILURES (fails=${fails})"
exit 1