#!/usr/bin/env bash
# Toxiproxy + nginx backend 起動（docker コマンドのみ）
set -euo pipefail

# shellcheck source=../lib/common.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/../lib" && pwd)/common.sh"

BACKEND_CONTAINER="${BACKEND_CONTAINER:-veil-sec-backend}"
TOXIPROXY_CONTAINER="${TOXIPROXY_CONTAINER:-veil-sec-toxiproxy}"
TOXIPROXY_IMAGE="${TOXIPROXY_IMAGE:-ghcr.io/shopify/toxiproxy:2.9.0}"
BACKEND_IMAGE="${BACKEND_IMAGE:-traefik/whoami}"
TOXIPROXY_API_PORT="${TOXIPROXY_API_PORT:-8474}"
TOXIPROXY_PROXY_PORT="${TOXIPROXY_PROXY_PORT:-8480}"

start_chaos_infra() {
    log "カオス基盤起動: backend + toxiproxy"
    docker pull "${BACKEND_IMAGE}" >/dev/null 2>&1 || true
    docker pull "${TOXIPROXY_IMAGE}" >/dev/null 2>&1 || true

    docker run -d \
        --name "${BACKEND_CONTAINER}" \
        --network "${NET_NAME}" \
        --network-alias "${BACKEND_CONTAINER}" \
        --hostname veil-backend \
        "${BACKEND_IMAGE}" >/dev/null

    local i
    for ((i = 1; i <= 30; i++)); do
        if docker run --rm --network "${NET_NAME}" --entrypoint curl "${HARNESS_IMAGE}" \
            -sf --max-time 2 "http://${BACKEND_CONTAINER}/" >/dev/null 2>&1; then
            log "Backend 応答 (${i}/30)"
            break
        fi
        sleep 1
        [[ "${i}" -eq 30 ]] && die "Backend が起動しませんでした"
    done

    docker run -d \
        --name "${TOXIPROXY_CONTAINER}" \
        --network "${NET_NAME}" \
        --network-alias "${TOXIPROXY_CONTAINER}" \
        --hostname veil-toxiproxy \
        "${TOXIPROXY_IMAGE}" \
        -host=0.0.0.0 >/dev/null

    for ((i = 1; i <= 30; i++)); do
        if docker run --rm --network "${NET_NAME}" --entrypoint curl "${HARNESS_IMAGE}" \
            -sf --max-time 2 "http://${TOXIPROXY_CONTAINER}:${TOXIPROXY_API_PORT}/version" >/dev/null 2>&1; then
            log "Toxiproxy API 応答 (${i}/30)"
            break
        fi
        sleep 1
        [[ "${i}" -eq 30 ]] && die "Toxiproxy が起動しませんでした"
    done

    docker run --rm --network "${NET_NAME}" --entrypoint curl "${HARNESS_IMAGE}" \
        -sf -X POST "http://${TOXIPROXY_CONTAINER}:${TOXIPROXY_API_PORT}/proxies" \
        -H "Content-Type: application/json" \
        -d "{\"name\":\"backend\",\"listen\":\"0.0.0.0:${TOXIPROXY_PROXY_PORT}\",\"upstream\":\"${BACKEND_CONTAINER}:80\",\"enabled\":true}" \
        >/dev/null

    docker run --rm --network "${NET_NAME}" --entrypoint curl "${HARNESS_IMAGE}" \
        -sf --max-time 5 "http://${TOXIPROXY_CONTAINER}:${TOXIPROXY_PROXY_PORT}/" \
        >/dev/null || die "Toxiproxy 経由の backend 到達性検証に失敗"

    log "Toxiproxy proxy backend -> ${BACKEND_CONTAINER}:80 (listen :${TOXIPROXY_PROXY_PORT})"
}

stop_chaos_infra() {
    docker rm -f "${BACKEND_CONTAINER}" "${TOXIPROXY_CONTAINER}" 2>/dev/null || true
}