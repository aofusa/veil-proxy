#!/usr/bin/env bash
# HTTP/3 (QUIC) 到達性プローブ（F-90 / P-03）
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_HTTP3_PORT="${VEIL_HTTP3_PORT:-443}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"
RESULTS="/results/http3_report.txt"

mkdir -p "$(dirname "${RESULTS}")"
: >"${RESULTS}"

log() { printf '%s\n' "$*" | tee -a "${RESULTS}"; }

fails=0

log "http3_probe start host=${VEIL_HOST} udp_port=${VEIL_HTTP3_PORT}"

# UDP ポート到達性（QUIC は UDP 443）
if timeout 3 bash -c "echo | nc -u -w2 ${VEIL_HOST} ${VEIL_HTTP3_PORT}" >/dev/null 2>&1; then
    log "udp_reachability: ok"
else
    # nc が ICMP エラーを返す場合も「ポートが開いている」可能性あり
    log "udp_reachability: inconclusive (continuing)"
fi

# quiche ベース HTTP/3 クライアント（P-03 本番検証）
# 既定 File ルート `/`（200 応答）。`/cached/` はプレフィックス末尾スラッシュのみで
# `/cached/index.html` にマッチしないため使用しない。
export HTTP3_PATH="${HTTP3_PATH:-/}"
h3_ok=0
if command -v http3-client >/dev/null 2>&1; then
    if http3-client; then
        log "http3_client: ok"
        h3_ok=1
    else
        log "WARN http3_client: failed (see http3_client_report.txt)"
    fi
else
    log "http3_client: missing binary"
fi

# curl --http3-only フォールバック
h3_code="000"
if [[ "${h3_ok}" -eq 0 ]] && curl --version 2>/dev/null | grep -qi http3; then
    h3_code=$(curl -sk --http3-only -o /dev/null -w "%{http_code}" --max-time 8 \
        "https://${VEIL_HOST}:${VEIL_HTTP3_PORT}/" 2>/dev/null || echo "000")
    log "curl_http3: code=${h3_code}"
    [[ "${h3_code}" =~ ^(200|301|302)$ ]] && h3_ok=1
fi
if [[ "${h3_ok}" -eq 1 ]]; then
    log "PASS http3_get"
fi

# プロセス生存（TLS 経路）
tls_code=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null || echo "000")
if [[ "${tls_code}" == "200" ]]; then
    log "post_http3_tls_health: ok"
else
    log "post_http3_tls_health: fail (${tls_code})"
    fails=$((fails + 1))
fi

# http3-client 成功を必須（B-34 修正後は TLS フォールバックのみの合格を廃止）
if [[ "${fails}" -eq 0 ]]; then
    if [[ "${h3_ok}" -eq 1 ]]; then
        log "http3: ok"
        exit 0
    fi
    log "http3: FAILURES (http3_client failed, curl_code=${h3_code})"
    exit 1
fi
log "http3: FAILURES (fails=${fails})"
exit 1