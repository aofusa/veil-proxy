#!/usr/bin/env bash
# cargo-fuzz 短時間実行（Rust コンテナ内、docker のみ）
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
RESULTS_DIR="${RESULTS_DIR:-${REPO_ROOT}/tools/container_security/results}"
RUST_IMAGE="${RUST_FUZZ_IMAGE:-rustlang/rust:nightly-bookworm}"
FUZZ_RUNS="${FUZZ_RUNS:-2000}"
FUZZ_MAX_TIME="${FUZZ_MAX_TIME:-120}"
FUZZ_TARGETS="${FUZZ_TARGETS:-hpack_decode config_toml http2_frame_decode http_header_validate http_request_smuggling}"
# WASM 系ターゲット（wasmtime 依存で重いため既定 off）。`--features wasm` で実体が動く。
#   wasm_abi      … 信頼境界外 .wasm バイト列のコンパイル境界（F-70）
#   wasm_host_abi … ゲスト→ホスト ABI マップ復元境界の冪等性（F-70 残件）
WASM_FUZZ_TARGETS="${WASM_FUZZ_TARGETS:-wasm_abi wasm_host_abi}"
RUN_WASM_FUZZ="${RUN_WASM_FUZZ:-0}"
REPORT="${RESULTS_DIR}/libfuzzer_report.txt"

mkdir -p "${RESULTS_DIR}"

log() { printf '%s\n' "$*" | tee -a "${REPORT}"; }

: >"${REPORT}"
log "libfuzzer start runs=${FUZZ_RUNS} max_time=${FUZZ_MAX_TIME}s"

docker run --rm \
    -v "${REPO_ROOT}:/app:rw" \
    -v "${RESULTS_DIR}:/results:rw" \
    -w /app \
    -e CARGO_TARGET_DIR=/app/tools/container_security/results/cargo-target \
    -e RUN_WASM_FUZZ="${RUN_WASM_FUZZ}" \
    -e WASM_FUZZ_TARGETS="${WASM_FUZZ_TARGETS}" \
    "${RUST_IMAGE}" \
    bash -c "
        set -euo pipefail
        rustup component add llvm-tools-preview 2>/dev/null || true
        cargo install cargo-fuzz --locked 2>/dev/null
        cd fuzz
        # F-80: 回帰 seed を cargo-fuzz 既定コーパス（fuzz/corpus/<target>/）へ複製。
        if [ -d regression_corpus ]; then
            for tdir in regression_corpus/*/; do
                [ -d \"\${tdir}\" ] || continue
                tname=\$(basename \"\${tdir}\")
                mkdir -p \"corpus/\${tname}\"
                cp -n \"\${tdir}\"* \"corpus/\${tname}/\" 2>/dev/null || true
            done
        fi
        for target in ${FUZZ_TARGETS}; do
            echo \"libfuzzer target=\${target}\" | tee -a /results/libfuzzer_report.txt
            cargo fuzz run \"\${target}\" -- -runs=${FUZZ_RUNS} -max_total_time=${FUZZ_MAX_TIME} \
                2>&1 | tee -a /results/libfuzzer_report.txt
        done
        if [ \"\${RUN_WASM_FUZZ}\" = 1 ]; then
            for target in \${WASM_FUZZ_TARGETS}; do
                echo \"libfuzzer target=\${target} (wasm)\" | tee -a /results/libfuzzer_report.txt
                cargo fuzz run \"\${target}\" --features wasm -- -runs=${FUZZ_RUNS} -max_total_time=${FUZZ_MAX_TIME} \
                    2>&1 | tee -a /results/libfuzzer_report.txt
            done
        fi
        echo libfuzzer: ok | tee -a /results/libfuzzer_report.txt
    "

log "libfuzzer 完了: ${REPORT}"