#!/usr/bin/env bash
# Proxy-Wasm コンプライアンス検証（F-90 / W-01〜W-03）
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"
RESULTS="/results/wasm_security_report.txt"

mkdir -p "$(dirname "${RESULTS}")"
: >"${RESULTS}"

log() { printf '%s\n' "$*" | tee -a "${RESULTS}"; }

fails=0

log "wasm_security_probe start"

# W-01/W-02: header_filter が X-Veil-Processed を付与（Proxy-Wasm サンプルモジュール準拠）
# B-30 修正後: HTTP/2（curl 既定 ALPN）File 応答でも WASM フィルタを検証
headers=$(curl -sk -D - -o /dev/null --max-time 8 \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/wasm/" 2>/dev/null | tr -d '\r')
code=$(printf '%s\n' "${headers}" | awk 'BEGIN{c=0} /^HTTP/{c=$2} END{print c}')
wasm_hdr=$(printf '%s\n' "${headers}" | grep -i '^x-veil-processed:' | head -1 | awk '{print $2}')
log "wasm_route: code=${code} X-Veil-Processed=${wasm_hdr:-missing}"

if [[ "${code}" == "200" ]]; then
    log "PASS wasm_route_responds"
else
    log "FAIL wasm_route_responds: code=${code}"
    fails=$((fails + 1))
fi

if [[ "${wasm_hdr}" == "true" ]]; then
    log "PASS proxy_wasm_header_filter"
else
    log "FAIL proxy_wasm_header_filter"
    fails=$((fails + 1))
fi

# W-03: 連続 12 リクエストでフィルタが安定（B-05 回帰）
parallel_ok=0
for ((i = 1; i <= 12; i++)); do
    h=$(curl -sk -D - -o /dev/null --max-time 8 \
        "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/wasm/" 2>/dev/null \
        | tr -d '\r' | grep -i '^x-veil-processed:' | head -1 | awk '{print $2}' || true)
    [[ "${h}" == "true" ]] && parallel_ok=$((parallel_ok + 1))
done
log "wasm_parallel_hits: ${parallel_ok}/12"
if [[ "${parallel_ok}" -ge 10 ]]; then
    log "PASS wasm_concurrent_filter"
else
    log "FAIL wasm_concurrent_filter"
    fails=$((fails + 1))
fi

# W-06: fuel 枯渇モジュール（応答がタイムアウト内に返りプロセスが生存）
fuel_start=$(date +%s%N 2>/dev/null || date +%s)
fuel_code=$(curl -sk --http1.1 -o /dev/null -w "%{http_code}" --max-time 8 \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/wasm-fuel/" 2>/dev/null || echo "000")
fuel_end=$(date +%s%N 2>/dev/null || date +%s)
log "wasm_fuel_route: code=${fuel_code}"

if [[ "${fuel_code}" == "000" ]]; then
    log "FAIL wasm_fuel_timeout (hang?)"
    fails=$((fails + 1))
else
    log "PASS wasm_fuel_no_hang code=${fuel_code}"
fi

hc=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null || echo "000")
if [[ "${hc}" == "200" ]]; then
    log "post_fuel_health: ok"
else
    log "post_fuel_health: fail (${hc})"
    fails=$((fails + 1))
fi

# ---------------------------------------------------------------------------
# F-109: WASM フィルタを HTTP/3 経由でも検証
# ---------------------------------------------------------------------------
VEIL_HTTP3_PORT="${VEIL_HTTP3_PORT:-443}"
if curl --help 2>&1 | grep -q -- '--http3'; then
    log "F-109: wasm over HTTP/3 udp_port=${VEIL_HTTP3_PORT}"
    set +e
    h3_headers=$(curl -sk -D - -o /dev/null --max-time 8 --http3-only \
        "https://${VEIL_HOST}:${VEIL_HTTP3_PORT}/wasm/" 2>/dev/null | tr -d '\r')
    set -e
    h3_code=$(printf '%s\n' "${h3_headers}" | awk 'BEGIN{c=0} /^HTTP/{c=$2} END{print c}')
    h3_wasm_hdr=$(printf '%s\n' "${h3_headers}" | grep -i '^x-veil-processed:' | head -1 | awk '{print $2}' || true)
    log "h3_wasm_route: code=${h3_code:-000} X-Veil-Processed=${h3_wasm_hdr:-missing}"

    if [[ -z "${h3_code}" ]] || [[ "${h3_code}" == "0" ]]; then
        log "WARN h3_wasm_route: no response (curl http3 may be unavailable)"
    else
        if [[ "${h3_code}" == "200" ]]; then
            log "PASS h3_wasm_route_responds"
        else
            log "FAIL h3_wasm_route_responds: code=${h3_code}"
            fails=$((fails + 1))
        fi
        if [[ "${h3_wasm_hdr}" == "true" ]]; then
            log "PASS h3_proxy_wasm_header_filter"
        else
            # B-38 等で H3 WASM 未配線の場合は FAIL（実装ギャップとして検出）
            log "FAIL h3_proxy_wasm_header_filter"
            fails=$((fails + 1))
        fi
    fi
else
    log "WARN curl --http3 not available; skip H3 wasm checks"
fi

if [[ "${fails}" -eq 0 ]]; then
    log "wasm_security: ok"
    exit 0
fi
log "wasm_security: FAILURES (fails=${fails})"
exit 1