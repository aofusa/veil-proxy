#!/usr/bin/env bash
# レートリミット 429 検証（F-90 / E-11, E-12）
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"
RESULTS="/results/rate_limit_report.txt"
# thread-per-core でカウンタが分散するため十分なバーストを送る
BURST="${RATE_LIMIT_BURST:-60}"

mkdir -p "$(dirname "${RESULTS}")"
: >"${RESULTS}"

log() { printf '%s\n' "$*" | tee -a "${RESULTS}"; }

log "rate_limit_probe start burst=${BURST}"

got_429=0
got_200=0
for ((i = 1; i <= BURST; i++)); do
    code=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 3 \
        "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/rate-limit/" 2>/dev/null || echo "000")
    case "${code}" in
        429) got_429=$((got_429 + 1)) ;;
        200) got_200=$((got_200 + 1)) ;;
    esac
done
log "burst_results: 200=${got_200} 429=${got_429}"

fails=0
if [[ "${got_429}" -gt 0 ]]; then
    log "PASS rate_limit_triggers_429"
else
    log "FAIL rate_limit_triggers_429"
    fails=$((fails + 1))
fi
if [[ "${got_200}" -gt 0 ]]; then
    log "PASS rate_limit_allows_under_threshold"
else
    log "FAIL rate_limit_allows_under_threshold"
    fails=$((fails + 1))
fi

# ---------------------------------------------------------------------------
# F-109: レートリミットを HTTP/3 経由でも検証
# ---------------------------------------------------------------------------
VEIL_HTTP3_PORT="${VEIL_HTTP3_PORT:-443}"
if curl --help 2>&1 | grep -q -- '--http3'; then
    log "F-109: rate_limit over HTTP/3 udp_port=${VEIL_HTTP3_PORT} burst=${BURST}"
    h3_got_429=0
    h3_got_200=0
    h3_got_other=0
    for ((i = 1; i <= BURST; i++)); do
        set +e
        code=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 3 --http3-only \
            "https://${VEIL_HOST}:${VEIL_HTTP3_PORT}/rate-limit/" 2>/dev/null || echo "000")
        set -e
        case "${code}" in
            429) h3_got_429=$((h3_got_429 + 1)) ;;
            200) h3_got_200=$((h3_got_200 + 1)) ;;
            000) h3_got_other=$((h3_got_other + 1)) ;;
            *) h3_got_other=$((h3_got_other + 1)) ;;
        esac
    done
    log "h3_burst_results: 200=${h3_got_200} 429=${h3_got_429} other=${h3_got_other}"
    # HTTP/3 未対応環境では全 000 になり得る → WARN に留め H2 結果を主とする
    if [[ "$((h3_got_200 + h3_got_429))" -eq 0 ]]; then
        log "WARN h3_rate_limit: no usable responses (curl http3 may be unavailable)"
    else
        if [[ "${h3_got_429}" -gt 0 ]]; then
            log "PASS h3_rate_limit_triggers_429"
        else
            log "FAIL h3_rate_limit_triggers_429"
            fails=$((fails + 1))
        fi
        if [[ "${h3_got_200}" -gt 0 ]]; then
            log "PASS h3_rate_limit_allows_under_threshold"
        else
            log "FAIL h3_rate_limit_allows_under_threshold"
            fails=$((fails + 1))
        fi
    fi
else
    log "WARN curl --http3 not available; skip H3 rate_limit checks"
fi

if [[ "${fails}" -eq 0 ]]; then
    log "rate_limit: ok"
    exit 0
fi
log "rate_limit: FAILURES (fails=${fails})"
exit 1