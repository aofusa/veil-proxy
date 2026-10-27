#!/usr/bin/env bash
# Prometheus メトリクス高頻度アクセス耐性（F-90 / E-13）
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"
METRICS_PATH="${METRICS_PATH:-/__metrics}"
RESULTS="/results/metrics_probe_report.txt"
REQUESTS="${METRICS_FLOOD_REQUESTS:-100}"

mkdir -p "$(dirname "${RESULTS}")"
: >"${RESULTS}"

log() { printf '%s\n' "$*" | tee -a "${RESULTS}"; }

log "metrics_probe start requests=${REQUESTS}"

metrics() { curl -sk --max-time 3 "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}${METRICS_PATH}" 2>/dev/null || true; }

ok=0
for ((i = 1; i <= REQUESTS; i++)); do
    # 本文を変数に受けてから調べる（`curl | grep -q` は pipefail の下で、本文が大きいと grep が先に終わって
    # curl が書き込みに失敗し、偽の失敗になる）
    body="$(metrics)"
    if grep -q 'veil_' <<<"${body}"; then
        ok=$((ok + 1))
    fi
done
log "metrics_ok: ${ok}/${REQUESTS}"

fails=0
if [[ "${ok}" -ge $((REQUESTS * 8 / 10)) ]]; then
    log "PASS metrics_flood_survives"
else
    log "FAIL metrics_flood_survives"
    fails=$((fails + 1))
fi

# B-158: でたらめな Host を送っても系列が増えないこと（以前は Host ごとに系列が増え、5000 種類で応答 10.8MB）
series() { grep -c '^veil_proxy_http_requests_total{' <<<"$(metrics)" || true; }
before="$(series)"
for ((i = 1; i <= 200; i++)); do
    curl -sk -o /dev/null --max-time 3 -H "Host: flood-${RANDOM}-${i}.invalid" \
        "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null || true
done
after="$(series)"
log "requests_total_series: before=${before} after=${after} (200 random Host headers)"
# 状態コードの種類の分だけは増え得るので、少しの余裕を見る。系列が 1 本も読めないのはメトリクスに届いていない
# （B-253: allowed_ips で 403 になっていても、before=0 after=0 で PASS と報告していた）
if [[ "${after}" -eq 0 ]]; then
    log "FAIL metrics_host_label_bounded (no veil_proxy_http_requests_total series readable)"
    fails=$((fails + 1))
elif [[ "${after}" -le $((before + 5)) ]]; then
    log "PASS metrics_host_label_bounded"
else
    log "FAIL metrics_host_label_bounded"
    fails=$((fails + 1))
fi

hc=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null || echo "000")
if [[ "${hc}" == "200" ]]; then
    log "post_flood_health: ok"
else
    log "post_flood_health: fail"
    fails=$((fails + 1))
fi

if [[ "${fails}" -eq 0 ]]; then
    log "metrics_probe: ok"
    exit 0
fi
log "metrics_probe: FAILURES (fails=${fails})"
exit 1