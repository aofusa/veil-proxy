#!/usr/bin/env bash
# 構造化アクセスログ有効時の負荷耐性（F-90 / E-15）
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"
RESULTS="/results/access_log_report.txt"
BURST="${ACCESS_LOG_BURST:-60}"

mkdir -p "$(dirname "${RESULTS}")"
: >"${RESULTS}"

log() { printf '%s\n' "$*" | tee -a "${RESULTS}"; }

fails=0

log "access_log_probe start burst=${BURST}"

ok=0
for ((i = 1; i <= BURST; i++)); do
    code=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
        "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/?access_log_probe=${i}" 2>/dev/null || echo "000")
    if [[ "${code}" == "200" ]]; then
        ok=$((ok + 1))
    fi
done

log "burst_success: ${ok}/${BURST}"
if [[ "${ok}" -ge $((BURST * 8 / 10)) ]]; then
    log "PASS access_log_burst"
else
    log "FAIL access_log_burst"
    fails=$((fails + 1))
fi

hc=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null || echo "000")
if [[ "${hc}" == "200" ]]; then
    log "post_probe_health: ok"
else
    log "post_probe_health: fail (${hc})"
    fails=$((fails + 1))
fi

if [[ "${fails}" -eq 0 ]]; then
    log "access_log: ok"
    exit 0
fi
log "access_log: FAILURES (fails=${fails})"
exit 1