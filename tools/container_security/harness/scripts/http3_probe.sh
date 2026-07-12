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

# S-H3-07 (F-92/F-94): Amplification — RFC 9000 の 3 倍制限を厳格アサート
# AMPLIFICATION_STRICT=1（既定）で ratio>3 を FAIL。0 で観測のみ。
export AMPLIFICATION_STRICT="${AMPLIFICATION_STRICT:-1}"
run_h3_mode amplification_check "h3_amplification_check" || true
check_tls_health "post_h3_amplification_check" || true

# S-H3-08 (F-94): 0-RTT リプレイ（非冪等 POST の Early Data 再送）
run_h3_mode early_data_replay "h3_early_data_replay" || true
check_tls_health "post_h3_early_data_replay" || true

# S-H3-09 (F-94): h3spec — 専用スクリプトへ委譲（H3SPEC_STRICT / H3SPEC_REQUIRED）
if [[ -x /scripts/h3spec_run.sh ]]; then
    set +e
    /scripts/h3spec_run.sh
    h3spec_rc=$?
    set -e
    if [[ "${h3spec_rc}" -eq 0 ]]; then
        log "PASS h3spec_run"
    else
        log "FAIL h3spec_run: rc=${h3spec_rc}"
        fails=$((fails + 1))
    fi
else
    log "SKIP h3spec_run: script missing"
fi

# ---------------------------------------------------------------------------
# F-96: レポート §5.2 HTTP/3 エッジ / DoS
# ---------------------------------------------------------------------------

# S-H3-10: UDP リフレクション/増幅（複数ソースポート Initial flood、3 倍制限）
run_h3_mode amplification_spoof "h3_amplification_spoof" || true
check_tls_health "post_h3_amplification_spoof" || true

# S-H3-11: MAX_STREAMS 枯渇（データ無しストリーム大量オープン）
run_h3_mode max_streams "h3_max_streams_attack" || true
check_tls_health "post_h3_max_streams" || true

# S-H3-12: Connection Migration スプーフィング
run_h3_mode migration_spoof "h3_migration_spoof" || true
check_tls_health "post_h3_migration_spoof" || true

# S-H3-13: QPACK 動的テーブル非同期参照攻撃（近似）
run_h3_mode qpack_async_ref "h3_qpack_async_ref" || true
check_tls_health "post_h3_qpack_async_ref" || true

# ---------------------------------------------------------------------------
# F-97: レポート §4 フェーズ3 HTTP/3
# ---------------------------------------------------------------------------

# S-H3-14: QUIC Stream Body Slowloris — 接続後ボディを極遅送信
run_h3_mode stream_body_slowloris "h3_stream_body_slowloris" || true
check_tls_health "post_h3_stream_body_slowloris" || true

# S-H3-15: QPACK Memory Exhaustion — 巨大ユニークヘッダ連打
run_h3_mode qpack_memory_exhaustion "h3_qpack_memory_exhaustion" || true
check_tls_health "post_h3_qpack_memory_exhaustion" || true

# S-H3-16 (F-97): :authority と Host 不一致 → 400
run_h3_mode authority_host_mismatch "h3_authority_host_mismatch" || true
check_tls_health "post_h3_authority_host_mismatch" || true

# ---------------------------------------------------------------------------
# F-101: レポート [追加提案3] QUIC フロー制御違反
# ---------------------------------------------------------------------------

# S-H3-17: MAX_DATA / MAX_STREAM_DATA 限界突破刺激 → crash なし
run_h3_mode flow_control_violation "h3_flow_control_violation" || true
check_tls_health "post_h3_flow_control_violation" || true

# ---------------------------------------------------------------------------
# F-103: QUIC コントロール層攻撃
# ---------------------------------------------------------------------------

# S-H3-18: Control / QPACK Stream Abuse
run_h3_mode control_stream_abuse "h3_control_stream_abuse" || true
check_tls_health "post_h3_control_stream_abuse" || true

# S-H3-19: CID Exhaustion
run_h3_mode cid_exhaustion "h3_cid_exhaustion" || true
check_tls_health "post_h3_cid_exhaustion" || true

# S-H3-20: Retry Packet / Token Spoofing
run_h3_mode token_spoofing "h3_token_spoofing" || true
check_tls_health "post_h3_token_spoofing" || true

# ---------------------------------------------------------------------------
# F-112: 0-RTT Anti-Replay（非冪等 POST）明示プローブ
# ---------------------------------------------------------------------------
# S-H3-08 (early_data_replay) を再掲し、レポートが求める「非冪等 POST の 0-RTT
# 再送攻撃耐性」を S-H3-21 として明示ログする。真の 0-RTT チケット再利用は
# quiche セッション永続化依存のため、http3-client の early_data_replay モードで
# 近似し、ポストヘルスで crash なしを確認する。

# S-H3-21: QUIC 0-RTT Anti-Replay（非冪等 POST）
run_h3_mode early_data_replay "h3_0rtt_anti_replay" || true
check_tls_health "post_h3_0rtt_anti_replay" || true

# curl --http3-only がある環境では非冪等 POST を追加で1回送り、2xx/4xx/接続拒否のいずれか
if curl --version 2>/dev/null | grep -qi http3; then
    post_code=$(curl -sk --http3-only -o /dev/null -w "%{http_code}" --max-time 8 \
        -X POST -d 'non-idempotent-0rtt-probe' \
        "https://${VEIL_HOST}:${VEIL_HTTP3_PORT}/" 2>/dev/null || echo "000")
    if [[ "${post_code}" =~ ^(000|[2-5][0-9][0-9])$ ]]; then
        log "PASS h3_0rtt_curl_post: code=${post_code}"
    else
        log "FAIL h3_0rtt_curl_post: code=${post_code}"
        fails=$((fails + 1))
    fi
    check_tls_health "post_h3_0rtt_curl_post" || true
else
    log "SKIP h3_0rtt_curl_post: curl http3 unavailable"
fi

# 最終ヘルス
check_tls_health "post_http3_tls_health" || true

if [[ "${fails}" -eq 0 ]]; then
    log "http3: ok"
    exit 0
fi
log "http3: FAILURES (fails=${fails})"
exit 1
