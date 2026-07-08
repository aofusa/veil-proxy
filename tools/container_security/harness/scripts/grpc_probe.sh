#!/usr/bin/env bash
# gRPC 不正フレーム・メタデータ検証（F-90 / P-06, P-07）
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

hc=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null || echo "000")
if [[ "${hc}" == "200" ]]; then
    log "post_probe_health: ok"
else
    log "post_probe_health: fail (${hc})"
    fails=$((fails + 1))
fi

if [[ "${fails}" -eq 0 ]]; then
    log "grpc: ok"
    exit 0
fi
log "grpc: FAILURES (fails=${fails})"
exit 1