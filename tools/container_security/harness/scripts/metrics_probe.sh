#!/usr/bin/env bash
# Prometheus メトリクス高頻度アクセス耐性（F-90 / E-13）
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"
METRICS_PATH="${METRICS_PATH:-/__metrics}"
RESULTS="/results/metrics_probe_report.txt"
REQUESTS="${METRICS_FLOOD_REQUESTS:-100}"

mkdir -p "$(dirname "${RESULTS}")"
: >"${RESULTS}"

log() { printf '%s\n' "$*" | tee -a "${RESULTS}"; }

log "metrics_probe start requests=${REQUESTS}"

ok=0
for ((i = 1; i <= REQUESTS; i++)); do
    if curl -sk --max-time 3 "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}${METRICS_PATH}" 2>/dev/null \
        | grep -q 'veil_'; then
        ok=$((ok + 1))
    fi
done
log "metrics_ok: ${ok}/${REQUESTS}"

fails=0
if [[ "${ok}" -ge $((REQUESTS * 8 / 10)) ]]; then
    log "PASS metrics_flood_survives"
else
    log "FAIL metrics_flood_survives"
    fails=$((fails + 1))
fi

hc=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null || echo "000")
if [[ "${hc}" == "200" ]]; then
    log "post_flood_health: ok"
else
    log "post_flood_health: fail"
    fails=$((fails + 1))
fi

if [[ "${fails}" -eq 0 ]]; then
    log "metrics_probe: ok"
    exit 0
fi
log "metrics_probe: FAILURES (fails=${fails})"
exit 1