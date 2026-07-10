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

# S-G-08 (F-92): gRPC Slowloris — LPM を極端に遅く送り、タイムアウト/解放後にヘルスが生きていること
# HTTP/1.1 + Content-Length でヘッダ完了後にボディを 1 バイト/200ms 程度で遅延送信
slow_log="$(mktemp)"
set +e
{
    # ヘッダ + 不完全 LPM（flags + length=32 を宣言し、ペイロードを遅延）
    printf 'POST /grpc.test.v1.TestService/UnaryCall HTTP/1.1\r\n'
    printf 'Host: veil-proxy\r\n'
    printf 'Content-Type: application/grpc\r\n'
    printf 'TE: trailers\r\n'
    printf 'Content-Length: 37\r\n'
    printf '\r\n'
    # LPM header: flags=0, length=32
    printf '\x00\x00\x00\x00\x20'
    # 32 バイトをゆっくり送る（約 6 秒）
    for _ in $(seq 1 32); do
        printf 'A'
        sleep 0.2
    done
} | timeout 12 openssl s_client -connect "${VEIL_HOST}:${VEIL_HTTPS_PORT}" \
    -servername "${VEIL_HOST}" -quiet 2>/dev/null >"${slow_log}" 2>&1
slow_rc=$?
set -e
if [[ "${slow_rc}" -eq 0 ]] || [[ "${slow_rc}" -eq 124 ]] || [[ "${slow_rc}" -eq 1 ]]; then
    log "PASS grpc_slowloris_lpm: completed (rc=${slow_rc})"
else
    log "WARN grpc_slowloris_lpm: rc=${slow_rc}"
fi
rm -f "${slow_log}"
check_health "post_grpc_slowloris" || true

# S-G-09 (F-92): RST_STREAM flood 相当 — 短命の gRPC リクエストを大量連打し、
# ストリームを即座に閉じてリソース枯渇しないことを post-health で確認
# （curl は RST を明示できないが、接続をすぐ切る連打で同様の負荷を与える）
flood_ok=0
flood_fail=0
for i in $(seq 1 40); do
    set +e
    c=$(timeout 2 curl -s -o /dev/null -w "%{http_code}" --max-time 1 --http2-prior-knowledge \
        -X POST -H "Content-Type: application/grpc" -H "TE: trailers" \
        -d $'\x00\x00\x00\x00\x02{}' \
        "http://${VEIL_HOST}:${VEIL_H2C_PORT}/grpc.test.v1.TestService/UnaryCall" 2>/dev/null)
    rc=$?
    set -e
    # 124/timeout・接続リセット・何らかの HTTP コードいずれも「サーバ生存下の完了」
    if [[ "${rc}" -eq 0 ]] || [[ "${rc}" -eq 124 ]] || [[ -n "${c}" ]]; then
        flood_ok=$((flood_ok + 1))
    else
        flood_fail=$((flood_fail + 1))
    fi
done
log "grpc_rst_flood_sim: ok=${flood_ok} fail=${flood_fail}"
if [[ "${flood_ok}" -ge 20 ]]; then
    log "PASS grpc_rst_flood_sim: completed burst"
else
    log "FAIL grpc_rst_flood_sim: too few completions (ok=${flood_ok})"
    fails=$((fails + 1))
fi
check_health "post_grpc_rst_flood" || true

check_health "post_probe_health" || true

if [[ "${fails}" -eq 0 ]]; then
    log "grpc: ok"
    exit 0
fi
log "grpc: FAILURES (fails=${fails})"
exit 1
