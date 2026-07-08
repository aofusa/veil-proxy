#!/usr/bin/env bash
# 管理 API (/__admin) 認証・認可バイパス検証（F-90 / E-01〜E-06）
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"
ADMIN_SECRET="${ADMIN_SECRET:-veil-sec-test-admin}"
RESULTS="/results/admin_security_report.txt"

mkdir -p "$(dirname "${RESULTS}")"
: >"${RESULTS}"

log() { printf '%s\n' "$*" | tee -a "${RESULTS}"; }

code_for() {
    local extra=("$@")
    curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
        "${extra[@]}" "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/" 2>/dev/null || echo "000"
}

admin_code() {
    local method="$1" path="$2"
    shift 2
    curl -sk -o /dev/null -w "%{http_code}" --max-time 5 -X "${method}" \
        "$@" "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}${path}" 2>/dev/null || echo "000"
}

fails=0
check() {
    local name="$1" want="$2" got="$3"
    if [[ "${got}" == "${want}" ]]; then
        log "PASS ${name}: expect=${want} got=${got}"
    else
        log "FAIL ${name}: expect=${want} got=${got}"
        fails=$((fails + 1))
    fi
}

log "admin_security_probe start"

c=$(admin_code GET "/__admin/config")
check "no_auth_config" "401" "${c}"

c=$(admin_code GET "/__admin/config" -H "Authorization: Bearer wrong-token")
check "bad_bearer_config" "401" "${c}"

c=$(admin_code GET "/__admin/config" -H "Authorization: Bearer ${ADMIN_SECRET}")
check "valid_bearer_config" "200" "${c}"

c=$(admin_code POST "/__admin/reload")
check "no_auth_reload" "401" "${c}"

c=$(admin_code POST "/__admin/reload" -H "Authorization: Bearer ${ADMIN_SECRET}")
check "valid_bearer_reload" "200" "${c}"

c=$(admin_code GET "/__admin/evil")
check "unknown_endpoint" "404" "${c}"

c=$(admin_code PUT "/__admin/config" -H "Authorization: Bearer ${ADMIN_SECRET}")
check "method_put_config" "404" "${c}"

# パストラバーサル風（admin プレフィックス外または未知サフィックス）
c=$(admin_code GET "/__admin/../etc/passwd")
check "path_traversal_admin" "404" "${c}"

# 事後ヘルス: 通常経路が生存
hc=$(code_for -X GET)
if [[ "${hc}" =~ ^(200|301|302)$ ]]; then
    log "post_probe_health: ok (${hc})"
else
    log "post_probe_health: fail (${hc})"
    fails=$((fails + 1))
fi

if [[ "${fails}" -eq 0 ]]; then
    log "admin_security: ok"
    exit 0
fi
log "admin_security: FAILURES (fails=${fails})"
exit 1