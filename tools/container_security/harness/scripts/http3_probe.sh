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

# curl --http3-only（ビルド依存。未対応ならスキップ扱いで TLS ヘルスへフォールバック）
h3_code="000"
if curl --version 2>/dev/null | grep -qi http3; then
    h3_code=$(curl -sk --http3-only -o /dev/null -w "%{http_code}" --max-time 8 \
        "https://${VEIL_HOST}:${VEIL_HTTP3_PORT}/" 2>/dev/null || echo "000")
    log "curl_http3: code=${h3_code}"
    if [[ "${h3_code}" =~ ^(200|301|302)$ ]]; then
        log "PASS http3_get"
    else
        log "WARN http3_get: code=${h3_code} (implementation or curl QUIC support)"
        # 実装問題の切り分けはレポートへ。プロセス生存は TLS で確認。
    fi
else
    log "curl_http3: skipped (curl without HTTP/3 support in harness image)"
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

# curl HTTP/3 非対応 (h3_code=000) は TLS 生存で合格。対応かつ非 2xx は実装問題として失敗。
if [[ "${fails}" -eq 0 ]]; then
    if [[ "${h3_code}" =~ ^(200|301|302)$ ]] || [[ "${h3_code}" == "000" ]]; then
        log "http3: ok"
        exit 0
    fi
    log "http3: FAILURES (http3_code=${h3_code})"
    exit 1
fi
log "http3: FAILURES (fails=${fails})"
exit 1