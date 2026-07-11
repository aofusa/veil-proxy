# QUIC/HTTP-3 対応 h2load をビルドするイメージ（local/h2load-h3:latest）。
#
# 既定の nghttp2 h2load（local/h2load）は QUIC/ngtcp2 を含まないため HTTP/3 を計測できない。
# 本イメージは quictls(OpenSSL+QUIC) / nghttp3 / ngtcp2 を組み込んだ nghttp2 を
# ソースからビルドし、--alpn-list=h3 で QUIC/HTTP-3 負荷をかけられる h2load を提供する。
#
# ビルド:  docker build -t local/h2load-h3:latest tools/perf/h2load-http3
# 使い方:  docker run --rm --entrypoint h2load local/h2load-h3:latest \
#            --alpn-list=h3 -n 30000 -c100 -m10 https://<host>:443/
FROM debian:bookworm-slim AS build
# バージョンは Debian bookworm の g++12 でビルドできる安定タグに固定する
# （nghttp2 の新しい版は C++23 の <print> を要求し bookworm では失敗するため）。
ARG OPENSSL_TAG=openssl-3.1.7+quic
ARG NGHTTP3_TAG=v1.4.0
ARG NGTCP2_TAG=v1.5.0
ARG NGHTTP2_TAG=v1.62.1
RUN apt-get update && apt-get install -y --no-install-recommends \
        git g++ make binutils autoconf automake autotools-dev libtool pkg-config \
        zlib1g-dev libcunit1-dev libssl-dev libxml2-dev libev-dev libevent-dev \
        libjansson-dev libc-ares-dev libjemalloc-dev libsystemd-dev \
        ca-certificates && rm -rf /var/lib/apt/lists/*

# quictls (QUIC 対応 OpenSSL フォーク)
RUN git clone --depth 1 -b ${OPENSSL_TAG} https://github.com/quictls/openssl /src/openssl \
    && cd /src/openssl \
    && ./config enable-tls1_3 --prefix=/usr/local --libdir=lib \
    && make -j"$(nproc)" && make install_sw

# nghttp3 (HTTP/3 フレーミング)
RUN git clone --depth 1 -b ${NGHTTP3_TAG} https://github.com/ngtcp2/nghttp3 /src/nghttp3 \
    && cd /src/nghttp3 && git submodule update --init \
    && autoreconf -i && ./configure --prefix=/usr/local --enable-lib-only \
    && make -j"$(nproc)" && make install

# ngtcp2 (QUIC トランスポート)
RUN git clone --depth 1 -b ${NGTCP2_TAG} https://github.com/ngtcp2/ngtcp2 /src/ngtcp2 \
    && cd /src/ngtcp2 && autoreconf -i \
    && ./configure PKG_CONFIG_PATH=/usr/local/lib/pkgconfig \
        LDFLAGS="-Wl,-rpath,/usr/local/lib" --prefix=/usr/local --enable-lib-only \
    && make -j"$(nproc)" && make install

# nghttp2 h2load (--enable-http3 で QUIC 負荷対応)
RUN git clone --depth 1 -b ${NGHTTP2_TAG} https://github.com/nghttp2/nghttp2 /src/nghttp2 \
    && cd /src/nghttp2 && git submodule update --init \
    && autoreconf -i \
    && ./configure --prefix=/usr/local --enable-app --enable-http3 --disable-python-bindings \
        PKG_CONFIG_PATH=/usr/local/lib/pkgconfig \
        LDFLAGS="-Wl,-rpath,/usr/local/lib" \
        OPENSSL_CFLAGS="-I/usr/local/include" \
        OPENSSL_LIBS="-L/usr/local/lib -lssl -lcrypto" \
        LIBNGHTTP3_CFLAGS="-I/usr/local/include" LIBNGHTTP3_LIBS="-L/usr/local/lib -lnghttp3" \
        LIBNGTCP2_CFLAGS="-I/usr/local/include" LIBNGTCP2_LIBS="-L/usr/local/lib -lngtcp2" \
        LIBNGTCP2_CRYPTO_QUICTLS_CFLAGS="-I/usr/local/include" \
        LIBNGTCP2_CRYPTO_QUICTLS_LIBS="-L/usr/local/lib -lngtcp2_crypto_quictls" \
    && make -j"$(nproc)" && make install

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        libev4 libjemalloc2 libc-ares2 ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/lib /usr/local/lib
COPY --from=build /usr/local/bin/h2load /usr/local/bin/h2load
ENV LD_LIBRARY_PATH=/usr/local/lib
ENTRYPOINT ["h2load"]
