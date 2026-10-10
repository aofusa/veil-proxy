# wrk（OpenSSL 3 リンク）— alloc_ab.sh の接続チャーン計測用（F-183）。
#
# 既定の williamyeh/wrk は Alpine 3.8 + LibreSSL 2.7（TLS 1.2 のみ、2018 年）で、
# 現行のクライアント（TLS 1.3 の PSK チケット再開）を代表しない。こちらは Debian の
# wrk（OpenSSL 3、TLS 1.3）。ビルド: docker build -t local/wrk-openssl:latest tools/perf/wrk-openssl
FROM debian:bookworm-slim
RUN apt-get update -qq && apt-get install -y -qq --no-install-recommends wrk ca-certificates \
    && rm -rf /var/lib/apt/lists/*
ENTRYPOINT ["wrk"]
