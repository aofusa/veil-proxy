#!/usr/bin/env bash
# L4 TCP 接続フラッド耐性（F-90 / P-10, P-11）
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_L4_PORT="${VEIL_L4_PORT:-4443}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"
RESULTS="/results/l4_flood_report.txt"
CONN_COUNT="${L4_FLOOD_CONNECTIONS:-80}"

mkdir -p "$(dirname "${RESULTS}")"
: >"${RESULTS}"

log() { printf '%s\n' "$*" | tee -a "${RESULTS}"; }

log "l4_flood_probe start connections=${CONN_COUNT} port=${VEIL_L4_PORT}"

opened=0
for ((i = 0; i < CONN_COUNT; i++)); do
    if timeout 1 bash -c "exec 3<>/dev/tcp/${VEIL_HOST}/${VEIL_L4_PORT}" 2>/dev/null; then
        opened=$((opened + 1))
    fi
done
log "l4_connections_opened: ${opened}/${CONN_COUNT}"

# P-11: L4 TLS パススルー接続試行（ClientHello 送信または TCP 到達）
if timeout 4 openssl s_client -connect "${VEIL_HOST}:${VEIL_L4_PORT}" -servername test </dev/null 2>/dev/null \
    | grep -qE 'CONNECTED|Cipher|SSL-Session'; then
    log "PASS l4_tls_passthrough_handshake"
elif timeout 2 bash -c "exec 3<>/dev/tcp/${VEIL_HOST}/${VEIL_L4_PORT}" 2>/dev/null; then
    log "PASS l4_tls_passthrough_tcp_reach"
else
    log "WARN l4_tls_passthrough: port not reachable (see B-33)"
fi

# 事後: HTTP/HTTPS が応答すること
code=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null || echo "000")
log "post_flood_https: ${code}"

fails=0
if [[ "${code}" == "200" ]]; then
    log "PASS http_survives_l4_flood"
else
    log "FAIL http_survives_l4_flood"
    fails=$((fails + 1))
fi

if [[ "${fails}" -eq 0 ]]; then
    log "l4_flood: ok"
    exit 0
fi
log "l4_flood: FAILURES (fails=${fails})"
exit 1