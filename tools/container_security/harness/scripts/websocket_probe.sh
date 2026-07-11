#!/usr/bin/env bash
# WebSocket 異常 Upgrade 検証（F-90 / P-08, P-09）
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"
RESULTS="/results/websocket_report.txt"

mkdir -p "$(dirname "${RESULTS}")"
: >"${RESULTS}"

log() { printf '%s\n' "$*" | tee -a "${RESULTS}"; }

fails=0
check_not_101() {
    local name="$1" code="$2"
    if [[ "${code}" == "101" ]]; then
        log "FAIL ${name}: unexpected 101 Switching Protocols"
        fails=$((fails + 1))
    else
        log "PASS ${name}: code=${code}"
    fi
}

log "websocket_probe start"

# 不正 Sec-WebSocket-Key
c=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
    -H "Upgrade: websocket" -H "Connection: Upgrade" \
    -H "Sec-WebSocket-Key: not-valid-base64!!!" \
    -H "Sec-WebSocket-Version: 13" \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/ws/" 2>/dev/null || echo "000")
check_not_101 "invalid_ws_key" "${c}"

# バージョン不正
c=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
    -H "Upgrade: websocket" -H "Connection: Upgrade" \
    -H "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==" \
    -H "Sec-WebSocket-Version: 99" \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/ws/" 2>/dev/null || echo "000")
check_not_101 "bad_ws_version" "${c}"

# Upgrade ヘッダ欠落
c=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
    -H "Connection: Upgrade" \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/ws/" 2>/dev/null || echo "000")
check_not_101 "missing_upgrade" "${c}"

# P-09: 仕様外コントロールフレーム（ws-probe Rust クライアント）
if command -v ws-probe >/dev/null 2>&1; then
    if ws-probe; then
        log "PASS ws_control_frame_probe"
    else
        log "FAIL ws_control_frame_probe"
        fails=$((fails + 1))
    fi
else
    log "WARN ws-probe binary missing (P-09 skipped)"
fi

# ---------------------------------------------------------------------------
# F-109: WebSocket over HTTP/3 (RFC 9220 Extended CONNECT) 不正リクエスト
# ---------------------------------------------------------------------------
VEIL_HTTP3_PORT="${VEIL_HTTP3_PORT:-443}"
run_h3_ws() {
    local mode="$1"
    local name="$2"
    if ! command -v http3-client >/dev/null 2>&1; then
        log "WARN ${name}: http3-client missing (skip H3 websocket)"
        return 0
    fi
    export VEIL_HOST
    export VEIL_SNI="${VEIL_SNI:-${VEIL_HOST}}"
    export VEIL_HTTP3_PORT
    export HTTP3_MODE="${mode}"
    export HTTP3_PATH="${HTTP3_PATH:-/ws/}"
    export HTTP3_REPORT="/results/http3_ws_${mode}_report.txt"
    set +e
    HTTP3_MODE="${mode}" http3-client
    local rc=$?
    set -e
    if [[ "${rc}" -eq 0 ]]; then
        log "PASS ${name}: http3-client mode=${mode}"
    else
        log "WARN ${name}: http3-client mode=${mode} rc=${rc} (continuing; health decides)"
    fi
}

log "F-109: WebSocket over HTTP/3 attack phase udp_port=${VEIL_HTTP3_PORT}"
run_h3_ws websocket_bad_connect "h3_ws_bad_connect"
run_h3_ws websocket_missing_protocol "h3_ws_missing_protocol"

# curl --http3-only があれば不正 Upgrade 風ヘッダも刺激（環境依存）
if curl --help 2>&1 | grep -q -- '--http3'; then
    set +e
    c=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 8 --http3-only \
        -X CONNECT \
        -H "Sec-WebSocket-Version: 99" \
        -H "Sec-WebSocket-Key: not-valid-base64!!!" \
        "https://${VEIL_HOST}:${VEIL_HTTP3_PORT}/ws/" 2>/dev/null || echo "000")
    set -e
    if [[ "${c}" == "000" ]]; then
        log "WARN h3_ws_bad_version_curl: no response (curl http3 may be unavailable)"
    else
        check_not_101 "h3_ws_bad_version_curl" "${c}"
    fi
else
    log "WARN curl --http3 not available; skip curl H3 websocket checks"
fi

# 事後ヘルス
hc=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null || echo "000")
if [[ "${hc}" == "200" ]]; then
    log "post_probe_health: ok"
else
    log "post_probe_health: fail (${hc})"
    fails=$((fails + 1))
fi

if [[ "${fails}" -eq 0 ]]; then
    log "websocket: ok"
    exit 0
fi
log "websocket: FAILURES (fails=${fails})"
exit 1