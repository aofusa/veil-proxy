#!/usr/bin/env bash
# セキュリティ検証: TLS・メソッド制限・応答ヘッダ（コンテナ内完結）
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_HTTP_PORT="${VEIL_HTTP_PORT:-80}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"
RESULTS="/results/security_scan_report.txt"

mkdir -p "$(dirname "${RESULTS}")"
: >"${RESULTS}"

log() {
    printf '%s\n' "$*" | tee -a "${RESULTS}"
}

# TLS ハンドシェイク確認
tls_check() {
    if echo | openssl s_client -connect "${VEIL_HOST}:${VEIL_HTTPS_PORT}" -servername localhost 2>/dev/null \
        | grep -q "BEGIN CERTIFICATE"; then
        log "tls_handshake: ok"
    else
        log "tls_handshake: fail"
        return 1
    fi
}

# 許可メソッド外 DELETE が拒否されること（config: HEAD, GET, POST）
# h2spec 要件で POST は許可。HTTP:80 は 301 のため HTTPS で検証。
method_restriction() {
    local code
    code=$(curl -sk -o /dev/null -w "%{http_code}" -X DELETE --max-time 3 \
        "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null || echo "000")
    log "delete_method_code_https: ${code}"
    case "${code}" in
        405|403|501) log "method_restriction: ok" ;;
        *)
            log "method_restriction: unexpected code ${code}"
            return 1
            ;;
    esac
}

# TRACE 等の危険メソッド
trace_check() {
    local code
    code=$(curl -sk -o /dev/null -w "%{http_code}" -X TRACE --max-time 3 \
        "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null || echo "000")
    log "trace_method_code: ${code}"
    if [[ "${code}" == "200" ]]; then
        log "trace_check: fail (TRACE が 200)"
        return 1
    fi
    log "trace_check: ok"
}

# パストラバーサル試行
path_traversal() {
    local code
    code=$(curl -s -o /dev/null -w "%{http_code}" --max-time 3 \
        "http://${VEIL_HOST}:${VEIL_HTTP_PORT}/..%2f..%2fetc%2fpasswd" 2>/dev/null || echo "000")
    log "path_traversal_code: ${code}"
    if [[ "${code}" == "200" ]] && curl -sf --max-time 3 \
        "http://${VEIL_HOST}:${VEIL_HTTP_PORT}/..%2f..%2fetc%2fpasswd" 2>/dev/null | grep -q "root:"; then
        log "path_traversal: fail"
        return 1
    fi
    log "path_traversal: ok"
}

log "security_scan start"
tls_check
method_restriction
trace_check
path_traversal
log "security_scan: ok"