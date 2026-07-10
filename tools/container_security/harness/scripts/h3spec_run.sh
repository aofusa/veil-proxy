#!/usr/bin/env bash
# h3spec による HTTP/3 / QUIC 準拠テスト（F-94）
# h2spec_run.sh と同等のゲート運用。CI 組み込みは F-95。
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_HTTP3_PORT="${VEIL_HTTP3_PORT:-443}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"
H3SPEC_TIMEOUT="${H3SPEC_TIMEOUT:-60}"
# H3SPEC_STRICT=1: h3spec 失敗をゲート失敗にする（既定 0 = 観測＋post-health のみ）
H3SPEC_STRICT="${H3SPEC_STRICT:-0}"
# H3SPEC_REQUIRED=1: バイナリ未導入を失敗にする（既定 0 = SKIP）
H3SPEC_REQUIRED="${H3SPEC_REQUIRED:-0}"
RESULTS="/results/h3spec_report.txt"

mkdir -p "$(dirname "${RESULTS}")"
: >"${RESULTS}"

log() {
    printf '%s\n' "$*" | tee -a "${RESULTS}"
}

check_tls_health() {
    local label="$1"
    local code
    code=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
        "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null || echo "000")
    if [[ "${code}" == "200" ]]; then
        log "PASS ${label}: tls_health=200"
        return 0
    fi
    log "FAIL ${label}: tls_health=${code}"
    return 1
}

log "h3spec_run start host=${VEIL_HOST} udp_port=${VEIL_HTTP3_PORT} strict=${H3SPEC_STRICT} required=${H3SPEC_REQUIRED}"

if ! command -v h3spec >/dev/null 2>&1; then
    if [[ "${H3SPEC_REQUIRED}" == "1" ]]; then
        log "FAIL h3spec: binary not installed (H3SPEC_REQUIRED=1)"
        exit 1
    fi
    log "SKIP h3spec: binary not installed (set H3SPEC_REQUIRED=1 to gate; see F-94 Dockerfile)"
    exit 0
fi

fails=0

# kazu-yamamoto/h3spec: 引数は `<host> <port>`（URL 形式ではない）
# 追加オプション: -n（証明書検証スキップ）、-t タイムアウト(ms)
set +e
timeout "${H3SPEC_TIMEOUT}" h3spec \
    -n \
    -t "$((H3SPEC_TIMEOUT * 1000))" \
    "${VEIL_HOST}" "${VEIL_HTTP3_PORT}" \
    >>"${RESULTS}" 2>&1
rc=$?
set -e

if [[ "${rc}" -eq 0 ]]; then
    log "PASS h3spec: exit=0"
elif [[ "${rc}" -eq 124 ]]; then
    log "WARN h3spec: timeout after ${H3SPEC_TIMEOUT}s"
    if [[ "${H3SPEC_STRICT}" == "1" ]]; then
        fails=$((fails + 1))
    fi
else
    log "WARN h3spec: exit=${rc} (see h3spec_report.txt)"
    if [[ "${H3SPEC_STRICT}" == "1" ]]; then
        log "FAIL h3spec: strict mode treats non-zero as failure"
        fails=$((fails + 1))
    fi
fi

if ! check_tls_health "post_h3spec"; then
    fails=$((fails + 1))
fi

if [[ "${fails}" -eq 0 ]]; then
    log "h3spec: ok"
    exit 0
fi
log "h3spec: FAILURES (fails=${fails})"
exit 1
