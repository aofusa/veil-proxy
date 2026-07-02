#!/usr/bin/env bash
# cargo-fuzz 短時間実行（Rust コンテナ内、docker のみ）
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
RESULTS_DIR="${RESULTS_DIR:-${REPO_ROOT}/tests/container_security/results}"
RUST_IMAGE="${RUST_FUZZ_IMAGE:-rustlang/rust:nightly-bookworm}"
FUZZ_RUNS="${FUZZ_RUNS:-2000}"
FUZZ_MAX_TIME="${FUZZ_MAX_TIME:-120}"
FUZZ_TARGETS="${FUZZ_TARGETS:-hpack_decode config_toml http2_frame_decode http_header_validate}"
REPORT="${RESULTS_DIR}/libfuzzer_report.txt"

mkdir -p "${RESULTS_DIR}"

log() { printf '%s\n' "$*" | tee -a "${REPORT}"; }

: >"${REPORT}"
log "libfuzzer start runs=${FUZZ_RUNS} max_time=${FUZZ_MAX_TIME}s"

docker run --rm \
    -v "${REPO_ROOT}:/app:rw" \
    -v "${RESULTS_DIR}:/results:rw" \
    -w /app \
    -e CARGO_TARGET_DIR=/app/tests/container_security/results/cargo-target \
    "${RUST_IMAGE}" \
    bash -c "
        set -euo pipefail
        rustup component add llvm-tools-preview 2>/dev/null || true
        cargo install cargo-fuzz --locked 2>/dev/null
        cd fuzz
        for target in ${FUZZ_TARGETS}; do
            echo \"libfuzzer target=\${target}\" | tee -a /results/libfuzzer_report.txt
            cargo fuzz run \"\${target}\" -- -runs=${FUZZ_RUNS} -max_total_time=${FUZZ_MAX_TIME} \
                2>&1 | tee -a /results/libfuzzer_report.txt
        done
        echo libfuzzer: ok | tee -a /results/libfuzzer_report.txt
    "

log "libfuzzer 完了: ${REPORT}"