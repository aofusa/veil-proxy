#!/usr/bin/env bash
# slowloris / 部分リクエストによる backpressure 検証
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_HTTP_PORT="${VEIL_HTTP_PORT:-80}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"
SLOWLORIS_COUNT="${SLOWLORIS_COUNT:-20}"
RESULTS="/results/slowloris_chaos_report.txt"

mkdir -p "$(dirname "${RESULTS}")"
: >"${RESULTS}"

log() { printf '%s\n' "$*" | tee -a "${RESULTS}"; }

log "slowloris_chaos start count=${SLOWLORIS_COUNT}"

# slowloris 風の部分リクエスト（ヘッダ途中で 0.2s 保持してから切断）を bash /dev/tcp で送る。
slow_ok=0
for ((n = 1; n <= SLOWLORIS_COUNT; n++)); do
    if { exec 3<>"/dev/tcp/${VEIL_HOST}/${VEIL_HTTP_PORT}"; } 2>/dev/null; then
        printf 'GET / HTTP/1.1\r\nHost: slowloris\r\n' >&3 2>/dev/null || true
        sleep 0.2
        exec 3>&- 3<&- 2>/dev/null || true
        slow_ok=$((slow_ok + 1))
    fi
done
log "slowloris_connections=${slow_ok}"

health_ok=0
for ((i = 1; i <= 10; i++)); do
    if curl -sf -o /dev/null --max-time 3 "http://${VEIL_HOST}:${VEIL_HTTP_PORT}/" 2>/dev/null; then
        health_ok=1
        break
    fi
    sleep 1
done
log "health_after_slowloris=${health_ok}"

https_ok=0
if curl -skf -o /dev/null --max-time 5 "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null; then
    https_ok=1
fi
log "https_after_slowloris=${https_ok}"

if [[ "${slow_ok}" -ge 1 ]] && [[ "${health_ok}" -eq 1 ]] && [[ "${https_ok}" -eq 1 ]]; then
    log "slowloris_chaos: ok"
    exit 0
fi
log "slowloris_chaos: failed"
exit 1