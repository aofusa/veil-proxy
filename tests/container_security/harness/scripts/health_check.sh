#!/usr/bin/env bash
# Veil の HTTP/HTTPS 応答確認（コンテナ内完結）
set -euo pipefail

VEIL_HOST="${VEIL_HOST:-veil-proxy}"
VEIL_HTTP_PORT="${VEIL_HTTP_PORT:-80}"
VEIL_HTTPS_PORT="${VEIL_HTTPS_PORT:-443}"

curl -sf -o /dev/null -w "%{http_code}" --max-time 5 \
    "http://${VEIL_HOST}:${VEIL_HTTP_PORT}/" | grep -qE '^(200|301|302)$'

# TLS は自己署名のため検証スキップ
curl -skf -o /dev/null --max-time 10 "https://${VEIL_HOST}:${VEIL_HTTPS_PORT}/"

echo "health: ok"