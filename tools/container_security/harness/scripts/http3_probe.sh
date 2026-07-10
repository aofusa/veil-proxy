#!/usr/bin/env bash
# HTTP/3 (QUIC) 到達性 + F-91 攻撃・異常系プローブ（P-03 / S-H3-*）
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_HTTP3_PORT="${VEIL_HTTP3_PORT:-443}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"
RESULTS="/results/http3_report.txt"

mkdir -p "$(dirname "${RESULTS}")"
: >"${RESULTS}"

log() { printf '%s\n' "$*" | tee -a "${RESULTS}"; }

fails=0

check_tls_health() {
    local label="$1"
    local tls_code
    tls_code=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
        "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null || echo "000")
    if [[ "${tls_code}" == "200" ]]; then
        log "PASS ${label}: tls_health=200"
        return 0
    fi
    log "FAIL ${label}: tls_health=${tls_code}"
    fails=$((fails + 1))
    return 1
}

run_h3_mode() {
    local mode="$1"
    local name="$2"
    if ! command -v http3-client >/dev/null 2>&1; then
        log "FAIL ${name}: http3-client missing"
        fails=$((fails + 1))
        return 1
    fi
    export HTTP3_MODE="${mode}"
    export HTTP3_PATH="${HTTP3_PATH:-/}"
    export HTTP3_REPORT="/results/http3_client_${mode}_report.txt"
    if HTTP3_MODE="${mode}" http3-client; then
        log "PASS ${name}: http3-client mode=${mode}"
        return 0
    fi
    log "FAIL ${name}: http3-client mode=${mode} rc=$?"
    fails=$((fails + 1))
    return 1
}

log "http3_probe start host=${VEIL_HOST} udp_port=${VEIL_HTTP3_PORT}"

# UDP ポート到達性
if timeout 3 bash -c "echo | nc -u -w2 ${VEIL_HOST} ${VEIL_HTTP3_PORT}" >/dev/null 2>&1; then
    log "udp_reachability: ok"
else
    log "udp_reachability: inconclusive (continuing)"
fi

# S-H3-01: 正常 GET
export HTTP3_PATH="${HTTP3_PATH:-/}"
h3_ok=0
if command -v http3-client >/dev/null 2>&1; then
    if HTTP3_MODE=get HTTP3_REPORT="/results/http3_client_report.txt" http3-client; then
        log "http3_client: ok"
        h3_ok=1
    else
        log "WARN http3_client: failed (see http3_client_report.txt)"
    fi
else
    log "http3_client: missing binary"
fi

h3_code="000"
if [[ "${h3_ok}" -eq 0 ]] && curl --version 2>/dev/null | grep -qi http3; then
    h3_code=$(curl -sk --http3-only -o /dev/null -w "%{http_code}" --max-time 8 \
        "https://${VEIL_HOST}:${VEIL_HTTP3_PORT}/" 2>/dev/null || echo "000")
    log "curl_http3: code=${h3_code}"
    [[ "${h3_code}" =~ ^(200|301|302)$ ]] && h3_ok=1
fi
if [[ "${h3_ok}" -eq 1 ]]; then
    log "PASS http3_get"
else
    log "FAIL http3_get"
    fails=$((fails + 1))
fi

# S-H3-02: QUIC Handshake Flood
run_h3_mode handshake_flood "h3_handshake_flood" || true
check_tls_health "post_h3_handshake_flood" || true

# S-H3-03: QPACK bomb
run_h3_mode qpack_bomb "h3_qpack_bomb" || true
check_tls_health "post_h3_qpack_bomb" || true

# S-H3-04: CID spoof
run_h3_mode cid_spoof "h3_cid_spoof" || true
check_tls_health "post_h3_cid_spoof" || true

# S-H3-05: Malformed frames
run_h3_mode malformed "h3_malformed_frames" || true
check_tls_health "post_h3_malformed_frames" || true

# S-H3-06 (F-92): QUIC Handshake Slowloris — Initial 後に未完了放置
run_h3_mode handshake_slowloris "h3_handshake_slowloris" || true
check_tls_health "post_h3_handshake_slowloris" || true

# S-H3-07 (F-92): Amplification / reflection 観測（増幅比ログ、crash なし）
run_h3_mode amplification_check "h3_amplification_check" || true
check_tls_health "post_h3_amplification_check" || true

# S-H3-08 (F-92): h3spec 相当は専用バイナリが無い場合が多い。
# 利用可能なら実行、無ければスキップ（h2spec と同様の位置づけ）。
if command -v h3spec >/dev/null 2>&1; then
    set +e
    timeout 60 h3spec "https://${VEIL_HOST}:${VEIL_HTTP3_PORT}" \
        >"/results/h3spec_report.txt" 2>&1
    h3_rc=$?
    set -e
    if [[ "${h3_rc}" -eq 0 ]]; then
        log "PASS h3spec: exit=0"
    else
        # 準拠失敗はログに残し、プロセス生存のみ必須（strict ではない）
        log "WARN h3spec: exit=${h3_rc} (see h3spec_report.txt)"
    fi
    check_tls_health "post_h3spec" || true
else
    log "SKIP h3spec: binary not installed (optional; see F-92)"
fi

# 最終ヘルス
check_tls_health "post_http3_tls_health" || true

if [[ "${fails}" -eq 0 ]]; then
    log "http3: ok"
    exit 0
fi
log "http3: FAILURES (fails=${fails})"
exit 1
