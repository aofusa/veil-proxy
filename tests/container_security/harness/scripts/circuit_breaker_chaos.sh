#!/usr/bin/env bash
# サーキットブレーカー発火・回復検証（Toxiproxy limit_data + Prometheus metrics）
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"
TOXIPROXY_HOST="${TOXIPROXY_HOST:-veil-sec-toxiproxy}"
TOXIPROXY_API_PORT="${TOXIPROXY_API_PORT:-8474}"
METRICS_PATH="${METRICS_PATH:-/__metrics}"
UPSTREAM_NAME="${UPSTREAM_NAME:-toxi-pool}"
FAILURE_THRESHOLD="${FAILURE_THRESHOLD:-3}"
RESULTS="/results/circuit_breaker_chaos_report.txt"

mkdir -p "$(dirname "${RESULTS}")"
: >"${RESULTS}"

log() { printf '%s\n' "$*" | tee -a "${RESULTS}"; }

api() {
    curl -sf -X "$1" "http://${TOXIPROXY_HOST}:${TOXIPROXY_API_PORT}$2" \
        -H "Content-Type: application/json" \
        ${3:+-d "$3"}
}

metrics_cb_state() {
    curl -sk --max-time 5 "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}${METRICS_PATH}" 2>/dev/null \
        | grep 'veil_circuit_breaker_state' \
        | grep "upstream=\"${UPSTREAM_NAME}\"" \
        | awk '{print $NF}' \
        | tail -1 || echo ""
}

flood_api() {
    local count="$1"
    local fail=0
    local i code
    for ((i = 1; i <= count; i++)); do
        code=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 3 \
            "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/api/" 2>/dev/null || echo "000")
        if [[ "${code}" =~ ^5 ]]; then
            fail=$((fail + 1))
        fi
    done
    echo "${fail}"
}

log "circuit_breaker_chaos start upstream=${UPSTREAM_NAME}"

baseline_code=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 10 \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/api/" 2>/dev/null || echo "000")
log "baseline_http_code=${baseline_code}"

api POST "/proxies/backend/toxics" \
    '{"name":"cb_reset","type":"limit_data","attributes":{"bytes":0}}' >/dev/null
log "reset_toxic: applied bytes=0"

upstream_failures=$(flood_api 12)
log "upstream_failures=${upstream_failures}"

cb_open=0
if [[ "${upstream_failures}" -ge "${FAILURE_THRESHOLD}" ]]; then
    cb_open=1
fi

for ((i = 1; i <= 10; i++)); do
    state=$(metrics_cb_state)
    log "poll_${i}_cb_state=${state:-unknown}"
    if [[ "${state}" == "1" || "${state}" == "2" ]]; then
        cb_open=1
        break
    fi
    sleep 1
done
log "circuit_breaker_tripped=${cb_open}"

api DELETE "/proxies/backend/toxics/cb_reset" >/dev/null
log "reset_toxic: removed"

recovered=0
for ((i = 1; i <= 15; i++)); do
    code=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
        "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/api/" 2>/dev/null || echo "000")
    if [[ "${code}" =~ ^(200|301|302)$ ]]; then
        recovered=1
        break
    fi
    sleep 1
done
log "recovered_after_toxic_removal=${recovered}"

if [[ "${baseline_code}" =~ ^(200|301|302)$ ]] && [[ "${cb_open}" -eq 1 ]] && [[ "${recovered}" -eq 1 ]]; then
    log "circuit_breaker_chaos: ok"
    exit 0
fi
log "circuit_breaker_chaos: failed (baseline=${baseline_code} tripped=${cb_open} recovered=${recovered} failures=${upstream_failures})"
exit 1