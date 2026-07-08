#!/usr/bin/env bash
# gRPC-Web 不正リクエスト耐性（grpc-full / P-06 拡張）
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"
RESULTS="/results/grpc_web_report.txt"

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

log "grpc_web_probe start"

# gRPC-Web バイナリ（5 バイト未満）
c=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
    -X POST -H "Content-Type: application/grpc-web" -H "Accept: application/grpc-web" \
    -d 'bad' \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/grpc.test.v1.TestService/UnaryCall" 2>/dev/null || echo "000")
check_no_crash "grpc_web_malformed_body" "${c}"

# gRPC-Web-Text（base64 不正）
c=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
    -X POST -H "Content-Type: application/grpc-web-text" -H "Accept: application/grpc-web-text" \
    -d '!!!not-base64!!!' \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/grpc.test.v1.TestService/UnaryCall" 2>/dev/null || echo "000")
check_no_crash "grpc_web_text_invalid_b64" "${c}"

# 巨大 grpc-timeout（gRPC-Web+proto）
big_timeout=$(printf '9%.0s' {1..4000})
c=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
    -X POST -H "Content-Type: application/grpc-web+proto" -H "TE: trailers" \
    -H "grpc-timeout: ${big_timeout}" \
    -d $'\x00\x00\x00\x00\x02{}' \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/grpc.test.v1.TestService/UnaryCall" 2>/dev/null || echo "000")
check_no_crash "grpc_web_oversized_metadata" "${c}"

hc=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null || echo "000")
if [[ "${hc}" == "200" ]]; then
    log "post_probe_health: ok"
else
    log "post_probe_health: fail (${hc})"
    fails=$((fails + 1))
fi

if [[ "${fails}" -eq 0 ]]; then
    log "grpc_web: ok"
    exit 0
fi
log "grpc_web: FAILURES (fails=${fails})"
exit 1