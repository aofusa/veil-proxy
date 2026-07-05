#!/usr/bin/env bash
# F-71: ThreadSanitizer 有効の cargo-fuzz 実行（Rust nightly コンテナ、docker のみ）。
# データ競合（executor のタスク管理・スレッドローカル/アトミック周辺）を検出する。
#
# 注意: libFuzzer の harness 自体は単一スレッドのため、TSAN が競合を捕捉するのは
# ターゲットが内部で複数スレッドを起こす経路（runtime のワーカー/offload 等）に限られる。
# 本スクリプトは TSAN パイプラインの基盤確立が目的（F-71）。既定は SKIP。
# MSAN（未初期化読み）は instrumented std（`-Zbuild-std`）が必須で別途対応する。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
RESULTS_DIR="${RESULTS_DIR:-${REPO_ROOT}/tools/container_security/results}"
CORPUS_DIR="${CORPUS_DIR:-${SCRIPT_DIR}/corpus}"
RUST_IMAGE="${RUST_FUZZ_IMAGE:-rustlang/rust:nightly-bookworm}"
FUZZ_RUNS="${FUZZ_RUNS:-4000}"
FUZZ_MAX_TIME="${FUZZ_MAX_TIME:-180}"
# TSAN 対象は競合が起こり得るターゲットに絞る（純デコーダは単一スレッドで無意味なため）。
FUZZ_TARGETS="${FUZZ_TARGETS_TSAN:-hpack_decode http2_frame_decode}"
REPORT="${RESULTS_DIR}/libfuzzer_tsan_report.txt"

mkdir -p "${RESULTS_DIR}" "${CORPUS_DIR}"

if [[ "${SKIP_LIBFUZZER_TSAN:-1}" == "1" ]]; then
    echo "libfuzzer_tsan: skipped (SKIP_LIBFUZZER_TSAN=1)" | tee "${REPORT}"
    exit 0
fi

# F-80: 回帰 seed を永続コーパスへ複製。
REGRESSION_CORPUS="${REPO_ROOT}/fuzz/regression_corpus"
if [[ "${SEED_REGRESSION_CORPUS:-1}" == "1" && -d "${REGRESSION_CORPUS}" ]]; then
    for tdir in "${REGRESSION_CORPUS}"/*/; do
        [[ -d "${tdir}" ]] || continue
        tname="$(basename "${tdir}")"
        mkdir -p "${CORPUS_DIR}/${tname}"
        cp -n "${tdir}"* "${CORPUS_DIR}/${tname}/" 2>/dev/null || true
    done
fi

: >"${REPORT}"
echo "libfuzzer_tsan start runs=${FUZZ_RUNS} max_time=${FUZZ_MAX_TIME}s targets=${FUZZ_TARGETS}" | tee -a "${REPORT}"

docker run --rm \
    -v "${REPO_ROOT}:/app:rw" \
    -v "${RESULTS_DIR}:/results:rw" \
    -v "${CORPUS_DIR}:/corpus:rw" \
    -w /app \
    -e CARGO_TARGET_DIR=/app/tools/container_security/results/cargo-target-tsan \
    "${RUST_IMAGE}" \
    bash -c "
        set -euo pipefail
        rustup component add llvm-tools-preview rust-src 2>/dev/null || true
        cargo install cargo-fuzz --locked 2>/dev/null
        cd fuzz
        for target in ${FUZZ_TARGETS}; do
            mkdir -p /corpus/\${target}
            echo \"libfuzzer_tsan target=\${target}\" | tee -a /results/libfuzzer_tsan_report.txt
            cargo fuzz run --sanitizer thread \"\${target}\" /corpus/\${target} -- \
                -runs=${FUZZ_RUNS} -max_total_time=${FUZZ_MAX_TIME} \
                2>&1 | tee -a /results/libfuzzer_tsan_report.txt
        done
        echo 'libfuzzer_tsan: ok' | tee -a /results/libfuzzer_tsan_report.txt
    "

echo "libfuzzer_tsan 完了: ${REPORT}（corpus: ${CORPUS_DIR}）" | tee -a "${REPORT}"
