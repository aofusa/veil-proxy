#!/usr/bin/env bash
# gRPC 不正フレーム・メタデータ・セキュリティ検証（F-90 / F-91 / P-06 / S-G-*）
# 依存: bash, curl, dd, timeout, openssl（python 不要）
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_H2C_PORT="${VEIL_H2C_PORT:-8443}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"
RESULTS="/results/grpc_report.txt"

mkdir -p "$(dirname "${RESULTS}")"
: >"${RESULTS}"

log() { printf '%s\n' "$*" | tee -a "${RESULTS}"; }

fails=0
check_no_crash() {
    local name="$1" code="$2"
    if [[ "${code}" == "000" ]]; then
        log "FAIL ${name}: no response (hang?)"
        fails=$((fails + 1))
    else
        log "PASS ${name}: code=${code}"
    fi
}

check_health() {
    local label="$1"
    local hc
    hc=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
        "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null || echo "000")
    if [[ "${hc}" == "200" ]]; then
        log "PASS ${label}: health=200"
        return 0
    fi
    log "FAIL ${label}: health=${hc}"
    fails=$((fails + 1))
    return 1
}

log "grpc_probe start"

# H2C: 不正 gRPC ペイロード（5 バイト未満）
c=$(curl -s -o /dev/null -w "%{http_code}" --max-time 5 --http2-prior-knowledge \
    -X POST -H "Content-Type: application/grpc" -H "TE: trailers" \
    -d 'bad' "http://${VEIL_HOST}:${VEIL_H2C_PORT}/grpc.test.v1.TestService/UnaryCall" 2>/dev/null || echo "000")
check_no_crash "h2c_malformed_grpc_body" "${c}"

# H2C: 巨大 grpc-timeout メタデータ
big_timeout=$(printf '9%.0s' {1..4000})
c=$(curl -s -o /dev/null -w "%{http_code}" --max-time 5 --http2-prior-knowledge \
    -X POST -H "Content-Type: application/grpc" -H "TE: trailers" \
    -H "grpc-timeout: ${big_timeout}" \
    -d $'\x00\x00\x00\x00\x02{}' \
    "http://${VEIL_HOST}:${VEIL_H2C_PORT}/grpc.test.v1.TestService/UnaryCall" 2>/dev/null || echo "000")
check_no_crash "h2c_oversized_metadata" "${c}"

# TLS: application/grpc だが HTTP/1.1 経路
c=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
    -X POST -H "Content-Type: application/grpc" \
    -d $'\x00\x00\x00\x00\x02{}' \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/grpc.test.v1.TestService/UnaryCall" 2>/dev/null || echo "000")
check_no_crash "tls_grpc_over_http1" "${c}"

# S-G-05: gRPC Header Spoofing（クライアントが grpc-status を付与）
c=$(curl -s -o /dev/null -w "%{http_code}" --max-time 5 --http2-prior-knowledge \
    -X POST -H "Content-Type: application/grpc" -H "TE: trailers" \
    -H "grpc-status: 0" -H "grpc-message: spoofed" \
    -d $'\x00\x00\x00\x00\x02{}' \
    "http://${VEIL_HOST}:${VEIL_H2C_PORT}/grpc.test.v1.TestService/UnaryCall" 2>/dev/null || echo "000")
check_no_crash "grpc_status_spoofing" "${c}"

c=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 --http2 \
    -X POST -H "Content-Type: application/grpc" -H "TE: trailers" \
    -H "grpc-status: 0" -H "grpc-message: spoofed" \
    -d $'\x00\x00\x00\x00\x02{}' \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/grpc.test.v1.TestService/UnaryCall" 2>/dev/null || echo "000")
check_no_crash "grpc_status_spoofing_tls" "${c}"

# S-G-06: Oversized gRPC Message
# flags=0 + length=0x00080000 (512KiB) + 64KiB body（宣言と不一致）
tmp_body="$(mktemp)"
{
    printf '\x00\x00\x08\x00\x00'
    dd if=/dev/zero bs=1024 count=64 2>/dev/null | tr '\0' 'A'
} >"${tmp_body}"
c=$(curl -s -o /dev/null -w "%{http_code}" --max-time 8 --http2-prior-knowledge \
    -X POST -H "Content-Type: application/grpc" -H "TE: trailers" \
    --data-binary @"${tmp_body}" \
    "http://${VEIL_HOST}:${VEIL_H2C_PORT}/grpc.test.v1.TestService/UnaryCall" 2>/dev/null || echo "000")
check_no_crash "grpc_oversized_message" "${c}"

c=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 10 \
    -X POST -H "Content-Type: application/grpc" -H "TE: trailers" \
    --data-binary @"${tmp_body}" \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/grpc.test.v1.TestService/UnaryCall" 2>/dev/null || echo "000")
check_no_crash "grpc_oversized_message_tls" "${c}"
rm -f "${tmp_body}"

# S-G-07: Infinite Streaming — Content-Length 大・ボディ未送信を openssl で保持
# サーバが切断 or クライアント timeout で終了し、その後ヘルスが生きていれば PASS
inf_log="$(mktemp)"
set +e
timeout 10 openssl s_client -connect "${VEIL_HOST}:${VEIL_HTTPS_PORT}" -servername "${VEIL_HOST}" \
    -quiet 2>/dev/null <<'EOF' >"${inf_log}" 2>&1
POST /grpc.test.v1.TestService/UnaryCall HTTP/1.1
Host: veil-proxy
Content-Type: application/grpc
TE: trailers
Content-Length: 1048576

EOF
inf_rc=$?
set -e
# 124 = timeout が切った（ストリーム保持）/ 0 = サーバ切断や完了
if [[ "${inf_rc}" -eq 0 ]] || [[ "${inf_rc}" -eq 124 ]] || [[ "${inf_rc}" -eq 1 ]]; then
    log "PASS grpc_infinite_streaming: completed (rc=${inf_rc})"
else
    log "WARN grpc_infinite_streaming: rc=${inf_rc}"
fi
rm -f "${inf_log}"
check_health "post_grpc_infinite_streaming" || true

check_health "post_probe_health" || true

if [[ "${fails}" -eq 0 ]]; then
    log "grpc: ok"
    exit 0
fi
log "grpc: FAILURES (fails=${fails})"
exit 1
