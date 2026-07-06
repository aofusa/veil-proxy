# F-78: OSS-Fuzz ビルドイメージ（Veil）
#
# base-builder-rust は nightly Rust + cargo-fuzz + libFuzzer 一式を提供する。
# Veil は cmake/nasm（aws-lc-rs 等のビルド）を要するため追加インストールする。
FROM gcr.io/oss-fuzz-base/base-builder-rust

RUN apt-get update && apt-get install -y --no-install-recommends \
    cmake nasm \
    && rm -rf /var/lib/apt/lists/*

# OSS-Fuzz は $SRC 配下にプロジェクトを配置し、build.sh を実行する。
COPY . $SRC/veil
WORKDIR $SRC/veil
COPY build.sh $SRC/
