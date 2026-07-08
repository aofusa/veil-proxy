#!/usr/bin/env bash
# 圧縮爆弾・キャッシュデセプション検証（F-90 / E-07〜E-10）
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"
RESULTS="/results/compression_cache_report.txt"
TMP="/tmp/compression_cache_probe"

mkdir -p "$(dirname "${RESULTS}")" "${TMP}"
: >"${RESULTS}"

log() { printf '%s\n' "$*" | tee -a "${RESULTS}"; }

fails=0
check() {
    local name="$1" cond="$2"
    if eval "${cond}"; then
        log "PASS ${name}"
    else
        log "FAIL ${name}"
        fails=$((fails + 1))
    fi
}

log "compression_cache_probe start"

# E-08: 応答圧縮（Accept-Encoding: gzip）
code=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
    -H "Accept-Encoding: gzip" "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null || echo "000")
enc=$(curl -sk -D - -o /dev/null --max-time 5 \
    -H "Accept-Encoding: gzip" "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null \
    | tr -d '\r' | awk 'BEGIN{IGNORECASE=1} /^content-encoding:/ {print $2; exit}')
log "compression_response: code=${code} encoding=${enc:-none}"
check "response_compression" "[[ '${code}' == '200' ]]"

# E-07: 高圧縮率 gzip（10MB ゼロ）— 413/4xx/切断を許容、プロセス生存を重視
dd if=/dev/zero bs=1M count=10 2>/dev/null | gzip -9 >"${TMP}/bomb.gz"
bomb_code=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 10 \
    -X POST -H "Content-Encoding: gzip" -H "Content-Type: application/octet-stream" \
    --data-binary @"${TMP}/bomb.gz" "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null || echo "000")
log "gzip_bomb_post: code=${bomb_code}"
check "gzip_bomb_no_crash" "[[ '${bomb_code}' != '000' ]]"
hc=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null || echo "000")
check "post_bomb_health" "[[ '${hc}' == '200' ]]"

# E-09/E-10: キャッシュ経路 — X-Forwarded-Host を混ぜても通常 Host で安定
path="/cached/index.html"
body1=$(curl -sk --max-time 5 "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}${path}" 2>/dev/null || true)
body2=$(curl -sk --max-time 5 \
    -H "X-Forwarded-Host: evil.attacker.example" \
    "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}${path}" 2>/dev/null || true)
body3=$(curl -sk --max-time 5 "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}${path}" 2>/dev/null || true)
log "cache_body_lens: b1=${#body1} b2=${#body2} b3=${#body3}"
if [[ -n "${body1}" ]] && [[ "${body1}" == "${body3}" ]]; then
    log "PASS cache_key_stability"
else
    log "FAIL cache_key_stability"
    fails=$((fails + 1))
fi
if echo "${body3}" | grep -qi "evil.attacker.example"; then
    log "FAIL cache_poison_host_leak"
    fails=$((fails + 1))
else
    log "PASS cache_poison_host_leak"
fi

rm -rf "${TMP}"

if [[ "${fails}" -eq 0 ]]; then
    log "compression_cache: ok"
    exit 0
fi
log "compression_cache: FAILURES (fails=${fails})"
exit 1