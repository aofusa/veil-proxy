#!/usr/bin/env bash
# Veil の HTTP/HTTPS 応答確認（コンテナ内完結）
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_HTTP_PORT="${VEIL_HTTP_PORT:-80}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"

for ((i = 1; i <= 5; i++)); do
    code=$(curl -sf -o /dev/null -w "%{http_code}" --max-time 3 \
        "http://${VEIL_HOST}:${VEIL_HTTP_PORT}/" 2>/dev/null || echo "000")
    if [[ "${code}" =~ ^(200|301|302)$ ]]; then
        break
    fi
    sleep 1
    [[ "${i}" -eq 5 ]] && exit 1
done

# TLS は自己署名のため検証スキップ
curl -skf -o /dev/null --max-time 5 "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/"

echo "health: ok"