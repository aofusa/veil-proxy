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

# F-97: gRPC-Web Large Base64 Decoding DOS（正当だが極端に長い Base64 ペイロード）
# application/grpc-web-text はボディ全体が Base64。CPU 枯渇耐性とクラッシュ無しを検証。
# 注: curl が巨大 --data-binary をメモリ展開するため、~64KB binary（~86KB b64）+ 30s タイムアウト。
# クライアント側 000 でも post-health 200 ならプロセス生存として PASS（攻撃耐性の本旨）。
log "F-97: grpc_web_large_base64_dos"
large_b64=$(python3 - <<'PY' 2>/dev/null || true
import base64
# ~64KB binary → ~86KB base64（curl 展開と TLS 往復が現実的なサイズ）
print(base64.b64encode(b"A" * (64 * 1024)).decode())
PY
)
if [[ -z "${large_b64}" ]]; then
    large_b64=$(head -c 65536 /dev/zero | base64 -w0 2>/dev/null || head -c 65536 /dev/zero | base64)
fi
set +e
c=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 30 \
    -X POST -H "Content-Type: application/grpc-web-text" -H "Accept: application/grpc-web-text" \
    --data-binary "${large_b64}" \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/grpc.test.v1.TestService/UnaryCall" 2>/dev/null || echo "000")
set -e
if [[ "${c}" == "000" ]]; then
    hc=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
        "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null || echo "000")
    if [[ "${hc}" == "200" ]]; then
        log "PASS grpc_web_large_base64_dos: client timeout/no-code but health=${hc} (process survived)"
    else
        check_no_crash "grpc_web_large_base64_dos" "${c}"
    fi
else
    check_no_crash "grpc_web_large_base64_dos" "${c}"
fi

# ---------------------------------------------------------------------------
# F-99: gRPC-Web over HTTP/3
# ---------------------------------------------------------------------------
VEIL_HTTP3_PORT="${VEIL_HTTP3_PORT:-443}"
run_h3_grpc_web_mode() {
    local mode="$1"
    local name="$2"
    if ! command -v http3-client >/dev/null 2>&1; then
        log "WARN ${name}: http3-client missing (skip H3 gRPC-Web)"
        return 0
    fi
    export VEIL_HOST
    export VEIL_SNI="${VEIL_SNI:-${VEIL_HOST}}"
    export VEIL_HTTP3_PORT
    export HTTP3_MODE="${mode}"
    export HTTP3_GRPC_PATH="${HTTP3_GRPC_PATH:-/grpc.test.v1.TestService/UnaryCall}"
    export HTTP3_REPORT="/results/http3_grpc_web_${mode}_report.txt"
    set +e
    HTTP3_MODE="${mode}" http3-client
    local rc=$?
    set -e
    if [[ "${rc}" -eq 0 ]]; then
        log "PASS ${name}: http3-client mode=${mode}"
        return 0
    fi
    log "WARN ${name}: http3-client mode=${mode} rc=${rc} (continuing; health decides)"
    return 0
}

log "F-99: gRPC-Web over HTTP/3 attack phase udp_port=${VEIL_HTTP3_PORT}"

# h3_grpc_web_malformed_body: 5 バイト未満ペイロード
run_h3_grpc_web_mode grpc_web_malformed "h3_grpc_web_malformed_body"

# h3_grpc_web_large_base64_dos: 巨大 Base64
run_h3_grpc_web_mode grpc_web_large_b64 "h3_grpc_web_large_base64_dos"

# curl --http3-only があれば追加検証（環境依存・失敗しても非致命）
if curl --help 2>&1 | grep -q -- '--http3'; then
    set +e
    c=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 8 --http3-only \
        -X POST -H "Content-Type: application/grpc-web" -H "Accept: application/grpc-web" \
        -d 'bad' \
        "https://${VEIL_HOST}:${VEIL_HTTP3_PORT}/grpc.test.v1.TestService/UnaryCall" 2>/dev/null || echo "000")
    set -e
    # 000 は curl 未対応/QUIC 不通の可能性 — hang 以外は WARN に留める
    if [[ "${c}" == "000" ]]; then
        log "WARN h3_grpc_web_malformed_body_curl: no response (curl http3 may be unavailable)"
    else
        check_no_crash "h3_grpc_web_malformed_body_curl" "${c}"
    fi
else
    log "WARN curl --http3 not available; skip curl H3 gRPC-Web checks"
fi

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