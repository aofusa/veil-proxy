#!/bin/bash -eu
# F-78: OSS-Fuzz ビルドスクリプト（Veil）
#
# cargo-fuzz で全ターゲットをビルドし、生成バイナリと seed コーパスを $OUT へ配置する。
# OSS-Fuzz 環境変数: $SRC（ソース）, $OUT（成果物）, $CARGO_BUILD_TARGET 等は
# base-builder-rust が設定済み。

cd "$SRC/veil"

# libFuzzer 版 fuzz ターゲットをビルド（ASAN は helper が --sanitizer で指定）。
cargo fuzz build -O

FUZZ_TARGET_DIR="$SRC/veil/fuzz/target/x86_64-unknown-linux-gnu/release"

# 非 WASM ターゲット（既定 features）。
for target in hpack_decode config_toml http2_frame_decode http_header_validate; do
    cp "$FUZZ_TARGET_DIR/$target" "$OUT/"
done

# WASM ターゲットは wasmtime 依存で重いが OSS-Fuzz では継続実行の価値が高い。
# feature 有効化で別途ビルド（失敗しても非 WASM ターゲットは提供する）。
if cargo fuzz build -O --features wasm wasm_abi wasm_host_abi 2>/dev/null; then
    for target in wasm_abi wasm_host_abi; do
        cp "$FUZZ_TARGET_DIR/$target" "$OUT/" || true
    done
fi

# F-80 回帰 seed を OSS-Fuzz の seed コーパス（<target>_seed_corpus.zip）として添付。
if [ -d "$SRC/veil/fuzz/regression_corpus" ]; then
    for tdir in "$SRC/veil/fuzz/regression_corpus"/*/; do
        [ -d "$tdir" ] || continue
        tname="$(basename "$tdir")"
        if [ -f "$OUT/$tname" ]; then
            (cd "$tdir" && zip -q "$OUT/${tname}_seed_corpus.zip" ./* 2>/dev/null) || true
        fi
    done
fi
