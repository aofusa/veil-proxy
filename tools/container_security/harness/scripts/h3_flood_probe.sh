#!/usr/bin/env bash
# F-279（B-222）: HTTP/3 の偽装 Initial の洪水の後も HTTP/3・HTTPS が応答すること。
# メモリの増え方は呼び出し側（run_full_features_security.sh）が docker stats で見る。
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_HTTP3_PORT="${VEIL_HTTP3_PORT:-443}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"
RESULTS="/results/h3_flood_report.txt"
export H3_FLOOD_CONNECTIONS="${H3_FLOOD_CONNECTIONS:-5000}"

mkdir -p "$(dirname "${RESULTS}")"
: >"${RESULTS}"
log() { printf '%s\n' "$*" | tee -a "${RESULTS}"; }

log "h3_flood_probe start host=${VEIL_HOST} connections=${H3_FLOOD_CONNECTIONS}"
fails=0
if HTTP3_MODE=initial_flood HTTP3_REPORT=/results/http3_client_initial_flood_report.txt http3-client; then
    log "PASS h3_initial_flood_sent"
else
    log "FAIL h3_initial_flood_sent"
    fails=$((fails + 1))
fi

# 洪水の直後でも正規のクライアントが繋がる（Retry・接続数の上限で正規の接続まで拒まない）
if HTTP3_MODE=get HTTP3_REPORT=/results/http3_client_after_flood_report.txt http3-client; then
    log "PASS h3_get_after_flood"
else
    log "FAIL h3_get_after_flood"
    fails=$((fails + 1))
fi
code=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null || echo "000")
if [[ "${code}" == "200" ]]; then
    log "PASS https_after_h3_flood"
else
    log "FAIL https_after_h3_flood: ${code}"
    fails=$((fails + 1))
fi

if [[ "${fails}" -eq 0 ]]; then
    log "h3_flood: ok"
    exit 0
fi
log "h3_flood: FAILURES (fails=${fails})"
exit 1
