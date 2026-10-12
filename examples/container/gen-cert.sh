#!/usr/bin/env bash
# compose.secrets.yaml 用の自己署名証明書を secrets/ に作る（検証用）。
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p secrets www
[ -f www/index.html ] || echo "hello from veil" > www/index.html
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
  -keyout secrets/key.pem -out secrets/cert.pem -days 30 \
  -subj "/CN=${1:-localhost}" -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" 2>/dev/null
# コンテナ内の非 root ユーザ（nonroot = 65532）が読めるようにする（Docker secrets の file: は
# ホストのパーミッションをそのまま使う）。
chmod 0644 secrets/cert.pem secrets/key.pem
echo "wrote secrets/cert.pem secrets/key.pem (CN=${1:-localhost})"
