#!/usr/bin/env bash
# カオスエンジニアリング: 高並行負荷・断続接続・遅延リクエスト（コンテナ内完結）
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_HTTP_PORT="${VEIL_HTTP_PORT:-80}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"
CONCURRENCY="${CHAOS_CONCURRENCY:-50}"
REQUESTS="${CHAOS_REQUESTS:-500}"
RESULTS="/results/chaos_report.txt"

log() { printf '%s\n' "$*"; }

# 並行 HTTP リクエスト
http_flood() {
    local workers=$((CONCURRENCY / 5))
    [[ "${workers}" -lt 1 ]] && workers=1
    local per_worker=$((REQUESTS / workers))
    local pids=()
    for ((w = 1; w <= workers; w++)); do
        (
            local ok=0 fail=0
            for ((i = 1; i <= per_worker; i++)); do
                if curl -sf -o /dev/null --max-time 3 "http://${VEIL_HOST}:${VEIL_HTTP_PORT}/" 2>/dev/null; then
                    ok=$((ok + 1))
                else
                    fail=$((fail + 1))
                fi
            done
            echo "http_flood_worker ok=${ok} fail=${fail}"
        ) &
        pids+=($!)
    done
    for pid in "${pids[@]}"; do wait "${pid}"; done
}

# 並行 HTTPS リクエスト
https_flood() {
    local ok=0 fail=0
    for ((i = 1; i <= REQUESTS / 5; i++)); do
        if curl -skf -o /dev/null --max-time 5 "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null; then
            ok=$((ok + 1))
        else
            fail=$((fail + 1))
        fi
    done
    echo "https_flood ok=${ok} fail=${fail}"
}

# 断続的な短接続（接続確立直後に切断）
connection_churn() {
    local count=0
    for ((i = 1; i <= 100; i++)); do
        if python3 - <<'PY' 2>/dev/null; then
import os, socket
h = os.environ["VEIL_HOST"]
p = int(os.environ["VEIL_HTTP_PORT"])
s = socket.create_connection((h, p), timeout=1)
s.send(b"GET / HTTP/1.1\r\nHost: x\r\n")
s.close()
PY
            count=$((count + 1))
        fi
    done
    echo "connection_churn success=${count}"
}

# 許可されていないメソッド POST の大量送信（HTTPS で 405 等が返ること）
method_stress() {
    local denied=0
    for ((i = 1; i <= 50; i++)); do
        code=$(curl -sk -o /dev/null -w "%{http_code}" -X POST --max-time 2 \
            "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null || echo "000")
        if [[ "${code}" == "405" || "${code}" == "403" || "${code}" == "501" ]]; then
            denied=$((denied + 1))
        fi
    done
    echo "method_stress denied=${denied}"
}

mkdir -p "$(dirname "${RESULTS}")"
{
    log "chaos start concurrency=${CONCURRENCY}"
    export VEIL_HOST VEIL_HTTP_PORT
    http_flood &
    https_flood &
    connection_churn &
    method_stress &
    wait
    log "chaos verifying health"
} | tee "${RESULTS}"

/scripts/health_check.sh
echo "chaos: ok"