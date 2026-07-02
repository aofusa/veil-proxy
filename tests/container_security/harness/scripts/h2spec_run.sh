#!/usr/bin/env bash
# h2spec による HTTP/2 準拠テスト（TLS ALPN h2 + H2C）
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"
VEIL_H2C_PORT="${VEIL_H2C_PORT:-8443}"
H2SPEC_TIMEOUT="${H2SPEC_TIMEOUT:-30}"
H2SPEC_STRICT="${H2SPEC_STRICT:-0}"
RESULTS="/results/h2spec_report.txt"
TLS_JUNIT="/results/h2spec_tls_junit.xml"
H2C_JUNIT="/results/h2spec_h2c_junit.xml"

mkdir -p "$(dirname "${RESULTS}")"
: >"${RESULTS}"

log() {
    printf '%s\n' "$*" | tee -a "${RESULTS}"
}

h2spec_args() {
    local -a args=(-h "${VEIL_HOST}" -o "${H2SPEC_TIMEOUT}" -P /)
    if [[ "${H2SPEC_STRICT}" == "1" ]]; then
        args+=(-S)
    fi
    printf '%s\n' "${args[@]}"
}

run_tls() {
    log "h2spec_tls start host=${VEIL_HOST} port=${VEIL_HTTPS_PORT}"
    local -a args
    mapfile -t args < <(h2spec_args)
    if h2spec "${args[@]}" -p "${VEIL_HTTPS_PORT}" -t -k -j "${TLS_JUNIT}" 2>&1 | tee -a "${RESULTS}"; then
        log "h2spec_tls: ok"
        return 0
    fi
    log "h2spec_tls: failed"
    return 1
}

run_h2c() {
    log "h2spec_h2c start host=${VEIL_HOST} port=${VEIL_H2C_PORT}"
    local -a args
    mapfile -t args < <(h2spec_args)
    if h2spec "${args[@]}" -p "${VEIL_H2C_PORT}" -j "${H2C_JUNIT}" 2>&1 | tee -a "${RESULTS}"; then
        log "h2spec_h2c: ok"
        return 0
    fi
    log "h2spec_h2c: failed"
    return 1
}

verify_h2_preface() {
    # H2C ポートが HTTP/2 接続プリフェースを受け付けるか簡易確認
    if python3 - <<'PY' 2>/dev/null; then
import os, socket
h = os.environ["VEIL_HOST"]
p = int(os.environ["VEIL_H2C_PORT"])
s = socket.create_connection((h, p), timeout=5)
s.send(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n")
s.settimeout(3)
data = s.recv(64)
s.close()
# 設定フレーム等が返れば HTTP/2 応答とみなす
exit(0 if data else 1)
PY
        log "h2c_preface: ok"
    else
        log "h2c_preface: fail"
        return 1
    fi
}

export VEIL_HOST VEIL_H2C_PORT

tls_ok=0
h2c_ok=0
verify_h2_preface || true

if run_tls; then tls_ok=1; fi
if run_h2c; then h2c_ok=1; fi

log "h2spec_summary tls_ok=${tls_ok} h2c_ok=${h2c_ok}"

if [[ "${tls_ok}" -eq 1 && "${h2c_ok}" -eq 1 ]]; then
    log "h2spec: ok"
    exit 0
fi

log "h2spec: failed (see ${RESULTS})"
exit 1