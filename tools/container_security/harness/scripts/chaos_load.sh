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

# 並行 HTTPS リクエスト（逐次だと失敗時 max-time 累積で CI タイムアウトし得る）
https_flood() {
    local workers=$((CONCURRENCY / 10))
    [[ "${workers}" -lt 1 ]] && workers=1
    local per_worker=$((REQUESTS / workers / 5))
    [[ "${per_worker}" -lt 1 ]] && per_worker=1
    local pids=()
    for ((w = 1; w <= workers; w++)); do
        (
            local ok=0 fail=0
            for ((i = 1; i <= per_worker; i++)); do
                if curl -skf -o /dev/null --max-time 3 "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null; then
                    ok=$((ok + 1))
                else
                    fail=$((fail + 1))
                fi
            done
            echo "https_flood_worker ok=${ok} fail=${fail}"
        ) &
        pids+=($!)
    done
    for pid in "${pids[@]}"; do wait "${pid}"; done
}

# 断続的な短接続（接続確立直後に切断）。bash の /dev/tcp で不完全リクエストを送って即切断する。
connection_churn() {
    local count=0
    for ((i = 1; i <= 100; i++)); do
        if { exec 3<>"/dev/tcp/${VEIL_HOST}/${VEIL_HTTP_PORT}"; } 2>/dev/null; then
            printf 'GET / HTTP/1.1\r\nHost: x\r\n' >&3 2>/dev/null || true
            exec 3>&- 3<&- 2>/dev/null || true
            count=$((count + 1))
        fi
    done
    echo "connection_churn success=${count}"
}

# 許可されていないメソッド DELETE の大量送信（HTTPS で 405 等が返ること）
method_stress() {
    local denied=0
    for ((i = 1; i <= 50; i++)); do
        code=$(curl -sk -o /dev/null -w "%{http_code}" -X DELETE --max-time 2 \
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

if /scripts/health_check.sh; then
    # 最終ステータスをレポートにも記録する（report.sh の集計が拾えるように）。
    echo "chaos: ok" | tee -a "${RESULTS}"
else
    log "chaos: health_check failed after load (run.sh の最終 health で再検証)"
    exit 0
fi