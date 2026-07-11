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
# curl の非 0 終了（rc=56 等の途中切断）も攻撃耐性の正常経路なので set -e で落とさない
dd if=/dev/zero bs=1M count=10 2>/dev/null | gzip -9 >"${TMP}/bomb.gz"
set +e
bomb_code=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 10 \
    -X POST -H "Content-Encoding: gzip" -H "Content-Type: application/octet-stream" \
    --data-binary @"${TMP}/bomb.gz" "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null)
bomb_rc=$?
set -e
# 空 or 000 は失敗扱い。接続リセットで body が無くても http_code が取れる場合あり
if [[ -z "${bomb_code}" ]] || [[ "${bomb_code}" == "000" ]]; then
    if [[ "${bomb_rc}" -ne 0 ]]; then
        # 途中切断はクラッシュ無しとして許容（生存は post_bomb_health で確認）
        bomb_code="reset"
    else
        bomb_code="000"
    fi
fi
log "gzip_bomb_post: code=${bomb_code} curl_rc=${bomb_rc}"
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

# ---------------------------------------------------------------------------
# F-109: 同一検証を HTTP/3 経由でも実行（curl --http3-only がある場合）
# ---------------------------------------------------------------------------
VEIL_HTTP3_PORT="${VEIL_HTTP3_PORT:-443}"
if curl --help 2>&1 | grep -q -- '--http3'; then
    log "F-109: compression/cache over HTTP/3 udp_port=${VEIL_HTTP3_PORT}"
    set +e
    h3_code=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 8 --http3-only \
        -H "Accept-Encoding: gzip" \
        "https://${VEIL_HOST}:${VEIL_HTTP3_PORT}/" 2>/dev/null || echo "000")
    set -e
    if [[ "${h3_code}" == "000" ]]; then
        log "WARN h3_response_compression: no response (curl http3 may be unavailable)"
    else
        log "h3_compression_response: code=${h3_code}"
        check "h3_response_compression" "[[ '${h3_code}' == '200' ]]"
    fi

    # キャッシュ経路の安定性（H3）
    set +e
    h3_body1=$(curl -sk --max-time 8 --http3-only \
        "https://${VEIL_HOST}:${VEIL_HTTP3_PORT}${path}" 2>/dev/null || true)
    h3_body2=$(curl -sk --max-time 8 --http3-only \
        -H "X-Forwarded-Host: evil.attacker.example" \
        "https://${VEIL_HOST}:${VEIL_HTTP3_PORT}${path}" 2>/dev/null || true)
    h3_body3=$(curl -sk --max-time 8 --http3-only \
        "https://${VEIL_HOST}:${VEIL_HTTP3_PORT}${path}" 2>/dev/null || true)
    set -e
    log "h3_cache_body_lens: b1=${#h3_body1} b2=${#h3_body2} b3=${#h3_body3}"
    if [[ -z "${h3_body1}" && -z "${h3_body3}" ]]; then
        log "WARN h3_cache_key_stability: empty bodies (http3 may be unavailable)"
    elif [[ -n "${h3_body1}" ]] && [[ "${h3_body1}" == "${h3_body3}" ]]; then
        log "PASS h3_cache_key_stability"
    else
        log "FAIL h3_cache_key_stability"
        fails=$((fails + 1))
    fi
    if echo "${h3_body3}" | grep -qi "evil.attacker.example"; then
        log "FAIL h3_cache_poison_host_leak"
        fails=$((fails + 1))
    else
        log "PASS h3_cache_poison_host_leak"
    fi
else
    log "WARN curl --http3 not available; skip H3 compression/cache checks"
fi

rm -rf "${TMP}"

if [[ "${fails}" -eq 0 ]]; then
    log "compression_cache: ok"
    exit 0
fi
log "compression_cache: FAILURES (fails=${fails})"
exit 1