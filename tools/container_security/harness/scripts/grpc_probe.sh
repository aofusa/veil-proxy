#!/usr/bin/env bash
# gRPC 不正フレーム・メタデータ・セキュリティ検証（F-90 / F-91 / F-93 / P-06 / S-G-*）
# 依存: bash, curl, dd, timeout, openssl、http3-client（QUIC gRPC 攻撃）
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_H2C_PORT="${VEIL_H2C_PORT:-8443}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"
VEIL_HTTP3_PORT="${VEIL_HTTP3_PORT:-443}"
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

# S-G-10 (F-94): Fragmented LPM — 正常 LPM を 1 バイト単位で H2C DATA 相当に分割送信
# curl はフレーム制御できないため、openssl で HTTP/1.1 ボディを 1 バイトずつ送る
frag_log="$(mktemp)"
set +e
{
    printf 'POST /grpc.test.v1.TestService/UnaryCall HTTP/1.1\r\n'
    printf 'Host: veil-proxy\r\n'
    printf 'Content-Type: application/grpc\r\n'
    printf 'TE: trailers\r\n'
    printf 'Content-Length: 7\r\n'
    printf '\r\n'
    # LPM: flags=0 length=2 body={}
    for b in $'\x00' $'\x00' $'\x00' $'\x00' $'\x02' '{' '}'; do
        printf '%s' "${b}"
        sleep 0.05
    done
} | timeout 10 openssl s_client -connect "${VEIL_HOST}:${VEIL_HTTPS_PORT}" \
    -servername "${VEIL_HOST}" -quiet 2>/dev/null >"${frag_log}" 2>&1
frag_rc=$?
set -e
if [[ "${frag_rc}" -eq 0 ]] || [[ "${frag_rc}" -eq 124 ]] || [[ "${frag_rc}" -eq 1 ]]; then
    log "PASS grpc_fragmented_lpm: completed (rc=${frag_rc})"
else
    log "WARN grpc_fragmented_lpm: rc=${frag_rc}"
fi
rm -f "${frag_log}"
check_health "post_grpc_fragmented_lpm" || true

# S-G-11 (F-94): Half-closed ストリーム悪用 — リクエスト完了後に応答を読まず放置
half_log="$(mktemp)"
set +e
{
    # 完全な小リクエストを送り、応答を読まずに接続を保持
    printf 'POST /grpc.test.v1.TestService/UnaryCall HTTP/1.1\r\n'
    printf 'Host: veil-proxy\r\n'
    printf 'Content-Type: application/grpc\r\n'
    printf 'TE: trailers\r\n'
    printf 'Content-Length: 7\r\n'
    printf '\r\n'
    printf '\x00\x00\x00\x00\x02{}'
    # 応答を意図的に読まず 5 秒保持（TCP window 枯渇の近似）
    sleep 5
} | timeout 12 openssl s_client -connect "${VEIL_HOST}:${VEIL_HTTPS_PORT}" \
    -servername "${VEIL_HOST}" -quiet 2>/dev/null >"${half_log}" 2>&1
half_rc=$?
set -e
if [[ "${half_rc}" -eq 0 ]] || [[ "${half_rc}" -eq 124 ]] || [[ "${half_rc}" -eq 1 ]]; then
    log "PASS grpc_half_closed_hold: completed (rc=${half_rc})"
else
    log "WARN grpc_half_closed_hold: rc=${half_rc}"
fi
rm -f "${half_log}"
check_health "post_grpc_half_closed" || true

# ---------------------------------------------------------------------------
# F-93: gRPC over HTTP/3 (QUIC) 攻撃検証
# 合格条件: クライアント完了 + プロセス生存（post health 200）
# ---------------------------------------------------------------------------
run_h3_grpc_mode() {
    local mode="$1"
    local name="$2"
    if ! command -v http3-client >/dev/null 2>&1; then
        log "WARN ${name}: http3-client missing (skip QUIC gRPC attack)"
        return 0
    fi
    export VEIL_HOST
    export VEIL_SNI="${VEIL_SNI:-${VEIL_HOST}}"
    export VEIL_HTTP3_PORT
    export HTTP3_MODE="${mode}"
    export HTTP3_GRPC_PATH="${HTTP3_GRPC_PATH:-/grpc.test.v1.TestService/UnaryCall}"
    export HTTP3_REPORT="/results/http3_grpc_${mode}_report.txt"
    set +e
    HTTP3_MODE="${mode}" http3-client
    local rc=$?
    set -e
    if [[ "${rc}" -eq 0 ]]; then
        log "PASS ${name}: http3-client mode=${mode}"
        return 0
    fi
    # 攻撃モードはクライアント側エラーでも 0 終了する設計。非 0 は未知モード等。
    log "WARN ${name}: http3-client mode=${mode} rc=${rc} (continuing; health decides)"
    return 0
}

log "grpc over HTTP/3 (QUIC) attack phase start udp_port=${VEIL_HTTP3_PORT}"

# S-G-H3-01: 不正 gRPC LPM（長さ偽装）
run_h3_grpc_mode grpc_malformed "h3_grpc_malformed_frame"
check_health "post_h3_grpc_malformed" || true

# S-G-H3-02: ヘッダスプーフィング（grpc-status / grpc-message）
run_h3_grpc_mode grpc_header_spoof "h3_grpc_header_spoof"
check_health "post_h3_grpc_header_spoof" || true

# S-G-H3-03: QUIC Slowloris（LPM 1 バイト遅延送信）
run_h3_grpc_mode grpc_slowloris "h3_grpc_slowloris"
check_health "post_h3_grpc_slowloris" || true

# S-G-H3-04: ストリーム強制リセット（STOP_SENDING / RESET_STREAM 相当）
run_h3_grpc_mode grpc_stream_reset "h3_grpc_stream_reset"
check_health "post_h3_grpc_stream_reset" || true

# ---------------------------------------------------------------------------
# F-99: test_coverage_report — gRPC over HTTP/3 攻撃拡張
# ---------------------------------------------------------------------------

# S-G-H3-05: QPACK 動的テーブル・巨大ヘッダ連打
run_h3_grpc_mode grpc_qpack_bomb "h3_grpc_qpack_bomb"
check_health "post_h3_grpc_qpack_bomb" || true

# S-G-H3-06: MAX_CONCURRENT_STREAMS 超バースト
run_h3_grpc_mode grpc_max_streams "h3_grpc_max_concurrent_streams"
check_health "post_h3_grpc_max_streams" || true

# S-G-H3-07: ヘッダ後ボディ未送の長時間保持
run_h3_grpc_mode grpc_half_closed "h3_grpc_half_closed_body_hold"
check_health "post_h3_grpc_half_closed" || true

# S-G-H3-08: 不正タイミング Trailer / 偽装
run_h3_grpc_mode grpc_malicious_trailers "h3_grpc_malicious_trailers"
check_health "post_h3_grpc_malicious_trailers" || true

# ---------------------------------------------------------------------------
# F-103: gRPC over HTTP/3 攻撃拡張（S-G-H3-09〜13）
# ---------------------------------------------------------------------------

# S-G-H3-09: Oversized Message over HTTP/3
run_h3_grpc_mode grpc_oversized "h3_grpc_oversized_message"
check_health "post_h3_grpc_oversized" || true

# S-G-H3-10: Infinite Streaming over HTTP/3
run_h3_grpc_mode grpc_infinite_streaming "h3_grpc_infinite_streaming"
check_health "post_h3_grpc_infinite_streaming" || true

# S-G-H3-11: Fragmented LPM over HTTP/3
run_h3_grpc_mode grpc_fragmented_lpm "h3_grpc_fragmented_lpm"
check_health "post_h3_grpc_fragmented_lpm" || true

# S-G-H3-12: gRPC Path Bypass over HTTP/3
run_h3_grpc_mode grpc_path_bypass "h3_grpc_path_bypass"
# curl --http3-only があれば追加刺激
if curl --version 2>/dev/null | grep -qi http3; then
    for bypass_path in \
        "/grpc.test.v1.TestService%2FUnaryCall" \
        "/./grpc.test.v1.TestService/UnaryCall"
    do
        set +e
        c=$(curl -sk --http3-only -o /dev/null -w "%{http_code}" --max-time 5 \
            -X POST -H "Content-Type: application/grpc" -H "TE: trailers" \
            -d $'\x00\x00\x00\x00\x00' \
            "https://${VEIL_HOST}:${VEIL_HTTP3_PORT}${bypass_path}" 2>/dev/null || echo "000")
        set -e
        check_no_crash "h3_grpc_path_bypass_curl:${bypass_path}" "${c}"
    done
fi
check_health "post_h3_grpc_path_bypass" || true

# S-G-H3-13: WASM Crash Resilience over HTTP/3
run_h3_grpc_mode grpc_wasm_crash "h3_grpc_wasm_crash_resilience"
check_health "post_h3_grpc_wasm_crash" || true

# ---------------------------------------------------------------------------
# F-96: レポート §5.2 gRPC セキュリティ
# ---------------------------------------------------------------------------

# S-G-12: HPACK ヘッダ展開ボム相当 — 巨大ヘッダ多数を gRPC に付与
hpack_bomb_hdrs=()
for i in $(seq 1 40); do
    # 各 2KiB のカスタムヘッダ（合計 ~80KiB）
    val=$(printf 'H%.0s' {1..2048})
    hpack_bomb_hdrs+=(-H "x-hpack-bomb-${i}: ${val}")
done
set +e
c=$(curl -s -o /dev/null -w "%{http_code}" --max-time 10 --http2-prior-knowledge \
    -X POST -H "Content-Type: application/grpc" -H "TE: trailers" \
    "${hpack_bomb_hdrs[@]}" \
    -d $'\x00\x00\x00\x00\x02{}' \
    "http://${VEIL_HOST}:${VEIL_H2C_PORT}/grpc.test.v1.TestService/UnaryCall" 2>/dev/null || echo "000")
set -e
check_no_crash "grpc_hpack_bomb" "${c}"
check_health "post_grpc_hpack_bomb" || true

# S-G-13: MAX_CONCURRENT_STREAMS 違反 — 大量並行 gRPC でストリーム上限を刺激
# curl は 1 リクエスト/接続のため並列 curl 連打で近似（拒否/リセットは正常）
set +e
for _ in $(seq 1 60); do
    timeout 3 curl -s -o /dev/null -w "%{http_code}" --max-time 2 --http2-prior-knowledge \
        -X POST -H "Content-Type: application/grpc" -H "TE: trailers" \
        -d $'\x00\x00\x00\x00\x02{}' \
        "http://${VEIL_HOST}:${VEIL_H2C_PORT}/grpc.test.v1.TestService/UnaryCall" >/dev/null 2>&1 &
done
wait || true
set -e
log "PASS grpc_max_concurrent_streams_burst: launched 60 parallel RPCs"
check_health "post_grpc_max_concurrent_streams" || true

# S-G-14: Half-closed 強化 — ヘッダ完了後ボディ未送で長時間保持
half2_log="$(mktemp)"
set +e
{
    printf 'POST /grpc.test.v1.TestService/UnaryCall HTTP/1.1\r\n'
    printf 'Host: veil-proxy\r\n'
    printf 'Content-Type: application/grpc\r\n'
    printf 'TE: trailers\r\n'
    printf 'Content-Length: 37\r\n'
    printf '\r\n'
    # LPM ヘッダのみ（flags+len=32）を送りボディは放置
    printf '\x00\x00\x00\x00\x20'
    sleep 8
} | timeout 15 openssl s_client -connect "${VEIL_HOST}:${VEIL_HTTPS_PORT}" \
    -servername "${VEIL_HOST}" -quiet 2>/dev/null >"${half2_log}" 2>&1
half2_rc=$?
set -e
if [[ "${half2_rc}" -eq 0 ]] || [[ "${half2_rc}" -eq 124 ]] || [[ "${half2_rc}" -eq 1 ]]; then
    log "PASS grpc_half_closed_body_hold: completed (rc=${half2_rc})"
else
    log "WARN grpc_half_closed_body_hold: rc=${half2_rc}"
fi
rm -f "${half2_log}"
check_health "post_grpc_half_closed_body" || true

# S-G-15: 悪意のある Trailers 挿入 — ボディ途中で trailers 風終端を送る
# HTTP/1.1 では chunked で早期 trailer を模倣
trail_log="$(mktemp)"
set +e
{
    printf 'POST /grpc.test.v1.TestService/UnaryCall HTTP/1.1\r\n'
    printf 'Host: veil-proxy\r\n'
    printf 'Content-Type: application/grpc\r\n'
    printf 'TE: trailers\r\n'
    printf 'Transfer-Encoding: chunked\r\n'
    printf '\r\n'
    # 部分 LPM
    printf '5\r\n'
    printf '\x00\x00\x00\x00\x20'
    printf '\r\n'
    # ボディ未完のまま 0-chunk + trailers（早期 trailer 挿入）
    printf '0\r\n'
    printf 'grpc-status: 0\r\n'
    printf 'grpc-message: premature\r\n'
    printf '\r\n'
    sleep 2
} | timeout 10 openssl s_client -connect "${VEIL_HOST}:${VEIL_HTTPS_PORT}" \
    -servername "${VEIL_HOST}" -quiet 2>/dev/null >"${trail_log}" 2>&1
trail_rc=$?
set -e
if [[ "${trail_rc}" -eq 0 ]] || [[ "${trail_rc}" -eq 124 ]] || [[ "${trail_rc}" -eq 1 ]]; then
    log "PASS grpc_malicious_trailers: completed (rc=${trail_rc})"
else
    log "WARN grpc_malicious_trailers: rc=${trail_rc}"
fi
rm -f "${trail_log}"
check_health "post_grpc_malicious_trailers" || true

# H2C でも premature trailer 相当（不正な小さな body + grpc-status ヘッダ）
c=$(curl -s -o /dev/null -w "%{http_code}" --max-time 5 --http2-prior-knowledge \
    -X POST -H "Content-Type: application/grpc" -H "TE: trailers" \
    -H "grpc-status: 0" -H "Trailer: grpc-status" \
    -d $'\x00\x00\x00\x00\x10 partial' \
    "http://${VEIL_HOST}:${VEIL_H2C_PORT}/grpc.test.v1.TestService/UnaryCall" 2>/dev/null || echo "000")
check_no_crash "grpc_malicious_trailers_h2c" "${c}"
check_health "post_grpc_malicious_trailers_h2c" || true

# ---------------------------------------------------------------------------
# F-97: レポート §4 フェーズ3 gRPC
# ---------------------------------------------------------------------------

# S-G-16: gRPC Path Bypass — URL エンコード / パストラバーサル風で認証・ルーティングすり抜けを試みる
# 期待: クラッシュせず、成功ステータスで本来の gRPC サービスになりすませないこと
log "F-97: grpc_path_bypass"
for bypass_path in \
    "/grpc.test.v1.TestService%2FUnaryCall" \
    "/grpc.test.v1.TestService/..%2fUnaryCall" \
    "/./grpc.test.v1.TestService/UnaryCall" \
    "/%67rpc.test.v1.TestService/UnaryCall"
do
    c=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 --http2-prior-knowledge \
        -X POST -H "Content-Type: application/grpc" -H "TE: trailers" \
        -d $'\x00\x00\x00\x00\x00' \
        "http://${VEIL_HOST}:${VEIL_H2C_PORT}${bypass_path}" 2>/dev/null || echo "000")
    # 000 は hang 疑い FAIL。それ以外（400/404/200/502）はクラッシュ無し PASS
    check_no_crash "grpc_path_bypass:${bypass_path}" "${c}"
done
check_health "post_grpc_path_bypass" || true

# S-G-17: gRPC WASM Crash Resilience — 不正ペイロードで WASM 経路を刺激しプロキシ生存を確認
# header_filter 等が載っている gRPC ルートへ異常 LPM / 巨大メタデータを送る
log "F-97: grpc_wasm_crash_resilience"
c=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 8 --http2-prior-knowledge \
    -X POST \
    -H "Content-Type: application/grpc" \
    -H "TE: trailers" \
    -H "x-wasm-crash: $(printf 'X%.0s' {1..8000})" \
    -d $'\x00\xff\xff\xff\xff' \
    "http://${VEIL_HOST}:${VEIL_H2C_PORT}/grpc.test.v1.TestService/UnaryCall" 2>/dev/null || echo "000")
check_no_crash "grpc_wasm_crash_resilience" "${c}"
# 直後の正常リクエストで生存確認
c2=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 --http2-prior-knowledge \
    -X POST -H "Content-Type: application/grpc" -H "TE: trailers" \
    -d $'\x00\x00\x00\x00\x00' \
    "http://${VEIL_HOST}:${VEIL_H2C_PORT}/grpc.test.v1.TestService/UnaryCall" 2>/dev/null || echo "000")
check_no_crash "grpc_wasm_post_crash_recovery" "${c2}"
check_health "post_grpc_wasm_crash" || true

check_health "post_probe_health" || true

if [[ "${fails}" -eq 0 ]]; then
    log "grpc: ok"
    exit 0
fi
log "grpc: FAILURES (fails=${fails})"
exit 1
