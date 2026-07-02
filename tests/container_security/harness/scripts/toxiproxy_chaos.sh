#!/usr/bin/env bash
# Toxiproxy による upstream 遅延カオス（コンテナ内完結）
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"
TOXIPROXY_HOST="${TOXIPROXY_HOST:-veil-sec-toxiproxy}"
TOXIPROXY_API_PORT="${TOXIPROXY_API_PORT:-8474}"
RESULTS="/results/toxiproxy_chaos_report.txt"

mkdir -p "$(dirname "${RESULTS}")"
: >"${RESULTS}"

log() { printf '%s\n' "$*" | tee -a "${RESULTS}"; }

api() {
    curl -sf -X "$1" "http://${TOXIPROXY_HOST}:${TOXIPROXY_API_PORT}$2" \
        -H "Content-Type: application/json" \
        ${3:+-d "$3"}
}

log "toxiproxy_chaos start"

# /api/ は veil-config の Proxy ルート（toxiproxy 経由）
baseline_code=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 10 \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/api/" 2>/dev/null || echo "000")
baseline_ms=$(curl -sk -o /dev/null -w "%{time_total}" --max-time 10 \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/api/" 2>/dev/null || echo "999")
log "baseline_http_code=${baseline_code} baseline_time_s=${baseline_ms}"

api POST "/proxies/backend/toxics" \
    '{"name":"latency","type":"latency","attributes":{"latency":2000,"jitter":200}}' >/dev/null
log "latency_toxic: applied 2000ms"

slow_code=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 15 \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/api/" 2>/dev/null || echo "000")
slow_ms=$(curl -sk -o /dev/null -w "%{time_total}" --max-time 15 \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/api/" 2>/dev/null || echo "999")
log "latency_http_code=${slow_code} latency_time_s=${slow_ms}"

api DELETE "/proxies/backend/toxics/latency" >/dev/null
log "latency_toxic: removed"

recovered=0
for ((i = 1; i <= 10; i++)); do
    code=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
        "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/api/" 2>/dev/null || echo "000")
    if [[ "${code}" =~ ^(200|301|302|404)$ ]]; then
        recovered=1
        break
    fi
    sleep 1
done
log "recovered_after_toxic_removal=${recovered}"

# 遅延注入後はベースラインより遅いこと（秒単位 float）
slow_ok=0
if awk -v s="${slow_ms}" -v b="${baseline_ms}" 'BEGIN { exit !(s > b + 0.5) }'; then
    slow_ok=1
fi
log "latency_injection_observed=${slow_ok}"

# 接続リセット相当（limit_data bytes=0 で即切断）
api POST "/proxies/backend/toxics" \
    '{"name":"reset","type":"limit_data","attributes":{"bytes":0}}' >/dev/null
log "reset_toxic: applied bytes=0"

down_code=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/api/" 2>/dev/null || echo "000")
log "reset_http_code=${down_code}"

api DELETE "/proxies/backend/toxics/reset" >/dev/null
log "reset_toxic: removed"

reset_recovered=0
for ((i = 1; i <= 10; i++)); do
    code=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
        "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/api/" 2>/dev/null || echo "000")
    if [[ "${code}" =~ ^(200|301|302|404)$ ]]; then
        reset_recovered=1
        break
    fi
    sleep 1
done
log "reset_recovered=${reset_recovered}"

if [[ "${recovered}" -eq 1 ]] && [[ "${reset_recovered}" -eq 1 ]] \
    && [[ "${baseline_code}" =~ ^(200|301|302)$ ]]; then
    log "toxiproxy_chaos: ok"
    exit 0
fi
log "toxiproxy_chaos: failed (baseline_code=${baseline_code} recovered=${recovered} reset_recovered=${reset_recovered})"
exit 1