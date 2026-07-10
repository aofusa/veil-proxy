# syntax=docker/dockerfile:1
# Veil 向けファジング・カオス・セキュリティ検証ハーネス
#
# 旧構成では python3 に依存していたが、ファジング／モックバックエンドは Rust バイナリ
# （veil-sec-tools）へ、生 TCP/TLS プローブは bash(/dev/tcp) + openssl へ置換した。

# --- Rust ツールビルドステージ（std のみ・依存ゼロ）---
FROM rust:1-slim-bookworm AS tools-builder
RUN apt-get update -y \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        clang \
        cmake \
        perl \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY tools/ /build/
RUN cargo build --release --bins

FROM debian:bookworm-slim

ARG H2SPEC_VERSION=2.6.0
# F-94: h3spec（kazu-yamamoto/h3spec）— HTTP/3 / QUIC エラーケース検証
ARG H3SPEC_VERSION=0.1.13

RUN apt-get update -y \
    && apt-get install -y --no-install-recommends \
        bash \
        ca-certificates \
        curl \
        gzip \
        netcat-openbsd \
        openssl \
    && curl -fsSL -o /tmp/h2spec.tar.gz \
        "https://github.com/summerwind/h2spec/releases/download/v${H2SPEC_VERSION}/h2spec_linux_amd64.tar.gz" \
    && tar -xzf /tmp/h2spec.tar.gz -C /usr/local/bin h2spec \
    && chmod +x /usr/local/bin/h2spec \
    && rm -f /tmp/h2spec.tar.gz \
    && curl -fsSL -o /usr/local/bin/h3spec \
        "https://github.com/kazu-yamamoto/h3spec/releases/download/v${H3SPEC_VERSION}/h3spec-linux-x86_64" \
    && chmod +x /usr/local/bin/h3spec \
    && rm -rf /var/lib/apt/lists/*

# Rust 製ツール（旧 python スクリプトの置換）を配置。
COPY --from=tools-builder /build/target/release/fuzz-http /usr/local/bin/fuzz-http
COPY --from=tools-builder /build/target/release/bad-backend /usr/local/bin/bad-backend
COPY --from=tools-builder /build/target/release/http3-client /usr/local/bin/http3-client
COPY --from=tools-builder /build/target/release/ws-probe /usr/local/bin/ws-probe

WORKDIR /work
COPY scripts/ /scripts/
COPY fixtures/ /fixtures/

RUN chmod +x /scripts/*.sh 2>/dev/null || true

ENV VEIL_HOST=veil-proxy \
    VEIL_HTTP_PORT=80 \
    VEIL_HTTPS_PORT=443

ENTRYPOINT ["/scripts/entrypoint.sh"]
