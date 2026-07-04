# syntax=docker/dockerfile:1
# Veil 向けファジング・カオス・セキュリティ検証ハーネス
FROM debian:bookworm-slim

ARG H2SPEC_VERSION=2.6.0

RUN apt-get update -y \
    && apt-get install -y --no-install-recommends \
        bash \
        ca-certificates \
        curl \
        openssl \
        python3 \
        python3-minimal \
    && curl -fsSL -o /tmp/h2spec.tar.gz \
        "https://github.com/summerwind/h2spec/releases/download/v${H2SPEC_VERSION}/h2spec_linux_amd64.tar.gz" \
    && tar -xzf /tmp/h2spec.tar.gz -C /usr/local/bin h2spec \
    && chmod +x /usr/local/bin/h2spec \
    && rm -f /tmp/h2spec.tar.gz \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /work
COPY scripts/ /scripts/
COPY fixtures/ /fixtures/

RUN chmod +x /scripts/*.sh /scripts/*.py 2>/dev/null || true

ENV VEIL_HOST=veil-proxy \
    VEIL_HTTP_PORT=80 \
    VEIL_HTTPS_PORT=443

ENTRYPOINT ["/scripts/entrypoint.sh"]