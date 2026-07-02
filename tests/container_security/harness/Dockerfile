# syntax=docker/dockerfile:1
# Veil 向けファジング・カオス・セキュリティ検証ハーネス
FROM debian:bookworm-slim

RUN apt-get update -y \
    && apt-get install -y --no-install-recommends \
        bash \
        ca-certificates \
        curl \
        openssl \
        python3 \
        python3-minimal \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /work
COPY scripts/ /scripts/
COPY fixtures/ /fixtures/

RUN chmod +x /scripts/*.sh /scripts/*.py 2>/dev/null || true

ENV VEIL_HOST=veil-proxy \
    VEIL_HTTP_PORT=80 \
    VEIL_HTTPS_PORT=443

ENTRYPOINT ["/scripts/entrypoint.sh"]