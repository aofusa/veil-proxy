#!/usr/bin/env bash
# h2spec による HTTP/2 準拠テスト（TLS ALPN h2 + H2C）
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"
VEIL_H2C_PORT="${VEIL_H2C_PORT:-8443}"
H2SPEC_TIMEOUT="${H2SPEC_TIMEOUT:-30}"
H2SPEC_STRICT="${H2SPEC_STRICT:-0}"
H2SPEC_FULL="${H2SPEC_FULL:-0}"
RESULTS="/results/h2spec_report.txt"
TLS_GATE_JUNIT="/results/h2spec_tls_gate_junit.xml"
H2C_GATE_JUNIT="/results/h2spec_h2c_gate_junit.xml"
TLS_FULL_JUNIT="/results/h2spec_tls_full_junit.xml"
H2C_FULL_JUNIT="/results/h2spec_h2c_full_junit.xml"

mkdir -p "$(dirname "${RESULTS}")"
: >"${RESULTS}"

log() {
    printf '%s\n' "$*" | tee -a "${RESULTS}"
}

base_args() {
    local -a args=(-h "${VEIL_HOST}" -o "${H2SPEC_TIMEOUT}" -P /)
    if [[ "${H2SPEC_STRICT}" == "1" ]]; then
        args+=(-S)
    fi
    printf '%s\n' "${args[@]}"
}

run_h2spec() {
    local label="$1"
    local port="$2"
    local use_tls="$3"
    local specs="$4"
    local junit="$5"

    log "${label} start host=${VEIL_HOST} port=${port} specs=${specs}"
    local -a args
    mapfile -t args < <(base_args)
    local -a cmd=(h2spec ${specs} "${args[@]}" -p "${port}" -j "${junit}")
    if [[ "${use_tls}" == "1" ]]; then
        cmd+=(-t -k)
    fi

    if "${cmd[@]}" 2>&1 | tee -a "${RESULTS}"; then
        log "${label}: ok"
        return 0
    fi
    log "${label}: failed"
    return 1
}

# H2C プリフェイスを bash /dev/tcp で送り、何らかの応答（SETTINGS フレーム等）が
# 返ることを確認する。SETTINGS フレームは 64B 未満のため、固定長読み取り（head -c 64）
# だと満たされずブロックしてタイムアウトする。dd の 1 回の read() で「到着した分だけ」
# を読み、バイト数 > 0 を成功とみなす。
verify_h2_preface() {
    local nbytes=0
    if { exec 3<>"/dev/tcp/${VEIL_HOST}/${VEIL_H2C_PORT}"; } 2>/dev/null; then
        # 接続プリフェイス（マジック）に続けて空の SETTINGS フレーム
        # （len=0 type=0x04 flags=0 stream=0）を送る。サーバはこれを受けて
        # 自身の SETTINGS フレームを返す（マジックだけでは応答しない実装がある）。
        printf 'PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\x00\x00\x00\x04\x00\x00\x00\x00\x00' >&3 2>/dev/null || true
        nbytes=$(timeout 3 dd bs=64 count=1 <&3 2>/dev/null | wc -c)
        exec 3>&- 3<&- 2>/dev/null || true
    fi
    if [[ "${nbytes}" -gt 0 ]]; then
        log "h2c_preface: ok (${nbytes} bytes)"
    else
        log "h2c_preface: fail"
        return 1
    fi
}

export VEIL_HOST VEIL_H2C_PORT

tls_gate=0
h2c_gate=0
tls_full=0
h2c_full=0

verify_h2_preface || true

# ゲート: generic + hpack（相互運用の必須セット。受け入れ条件を満たす）
if run_h2spec "h2spec_tls_gate" "${VEIL_HTTPS_PORT}" 1 "generic hpack" "${TLS_GATE_JUNIT}"; then
    tls_gate=1
fi
if run_h2spec "h2spec_h2c_gate" "${VEIL_H2C_PORT}" 0 "generic hpack" "${H2C_GATE_JUNIT}"; then
    h2c_gate=1
fi

# 拡張: RFC 7540/7541 全件（時間がかかる。失敗は記録し H2SPEC_STRICT=1 のときのみゲート失敗）
if [[ "${H2SPEC_FULL}" == "1" ]]; then
    if run_h2spec "h2spec_tls_full" "${VEIL_HTTPS_PORT}" 1 "" "${TLS_FULL_JUNIT}"; then
        tls_full=1
    fi
    if run_h2spec "h2spec_h2c_full" "${VEIL_H2C_PORT}" 0 "" "${H2C_FULL_JUNIT}"; then
        h2c_full=1
    fi
else
    log "h2spec_full skipped (H2SPEC_FULL=0)"
    tls_full=1
    h2c_full=1
fi

log "h2spec_summary tls_gate=${tls_gate} h2c_gate=${h2c_gate} tls_full=${tls_full} h2c_full=${h2c_full}"

gate_ok=0
[[ "${tls_gate}" -eq 1 && "${h2c_gate}" -eq 1 ]] && gate_ok=1

full_ok=0
[[ "${tls_full}" -eq 1 && "${h2c_full}" -eq 1 ]] && full_ok=1

if [[ "${gate_ok}" -eq 1 ]]; then
    if [[ "${H2SPEC_STRICT}" == "1" && "${full_ok}" -eq 0 ]]; then
        log "h2spec: gate ok, full suite failed (H2SPEC_STRICT=1)"
        exit 1
    fi
    log "h2spec: ok"
    exit 0
fi

log "h2spec: failed (generic/hpack gate did not pass)"
exit 1