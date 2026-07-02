#!/usr/bin/env bash
# サーキットブレーカー発火・回復検証（Toxiproxy timeout + Prometheus metrics）
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"
TOXIPROXY_HOST="${TOXIPROXY_HOST:-veil-sec-toxiproxy}"
TOXIPROXY_API_PORT="${TOXIPROXY_API_PORT:-8474}"
METRICS_PATH="${METRICS_PATH:-/__metrics}"
UPSTREAM_NAME="${UPSTREAM_NAME:-toxi-pool}"
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
        | awk -v up="${UPSTREAM_NAME}" '
            /veil_circuit_breaker_state\{/ {
                if ($0 ~ "upstream=\"" up "\"") {
                    gsub(/.* /, "", $0)
                    print $0
                    exit
                }
            }
        ' || echo ""
}

flood_api() {
    local count="$1"
    local i
    for ((i = 1; i <= count; i++)); do
        curl -sk -o /dev/null --max-time 3 \
            "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/api/" 2>/dev/null || true
    done
}

log "circuit_breaker_chaos start upstream=${UPSTREAM_NAME}"

baseline_code=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 10 \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/api/" 2>/dev/null || echo "000")
log "baseline_http_code=${baseline_code}"

api POST "/proxies/backend/toxics" \
    '{"name":"cb_timeout","type":"timeout","attributes":{"timeout":100}}' >/dev/null
log "timeout_toxic: applied 100ms"

flood_api 12

cb_open=0
for ((i = 1; i <= 15; i++)); do
    state=$(metrics_cb_state)
    log "poll_${i}_cb_state=${state:-unknown}"
    if [[ "${state}" == "1" || "${state}" == "2" ]]; then
        cb_open=1
        break
    fi
    sleep 1
done
log "circuit_breaker_tripped=${cb_open}"

api DELETE "/proxies/backend/toxics/cb_timeout" >/dev/null
log "timeout_toxic: removed"

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
log "circuit_breaker_chaos: failed (baseline=${baseline_code} tripped=${cb_open} recovered=${recovered})"
exit 1