#!/usr/bin/env bash
# OpenTelemetry OTLP 到達不能時の生存検証（F-90 / E-14）
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"
RESULTS="/results/otel_probe_report.txt"
WAIT_SEC="${OTEL_PROBE_WAIT_SEC:-12}"

mkdir -p "$(dirname "${RESULTS}")"
: >"${RESULTS}"

log() { printf '%s\n' "$*" | tee -a "${RESULTS}"; }

log "otel_probe start wait=${WAIT_SEC}s (endpoint unreachable in fixture)"

# fixture は endpoint=127.0.0.1:1 — batch_interval_secs=5 で数回送信試行が走る
sleep "${WAIT_SEC}"

code=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null || echo "000")
log "post_otel_wait_https: ${code}"

if [[ "${code}" == "200" ]]; then
    log "otel_probe: ok"
    exit 0
fi
log "otel_probe: FAILURES"
exit 1