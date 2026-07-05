#!/usr/bin/env bash
# AddressSanitizer 有効の cargo-fuzz 実行（Rust nightly コンテナ、docker のみ）。
# io_uring / kTLS / ゼロコピー周辺の unsafe に潜むメモリ不正使用（heap-buffer-overflow,
# use-after-free 等）を検出する。ASAN は cargo-fuzz の既定 sanitizer だが、本スクリプトでは
# 明示指定 + 永続コーパス（corpus/<target>/）でカバレッジを回次間で再利用する。
#
# 通常の run_libfuzzer.sh より重いため既定では suite から除外（SKIP_LIBFUZZER_ASAN=1）。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
RESULTS_DIR="${RESULTS_DIR:-${REPO_ROOT}/tools/container_security/results}"
CORPUS_DIR="${CORPUS_DIR:-${SCRIPT_DIR}/corpus}"
RUST_IMAGE="${RUST_FUZZ_IMAGE:-rustlang/rust:nightly-bookworm}"
FUZZ_RUNS="${FUZZ_RUNS:-4000}"
FUZZ_MAX_TIME="${FUZZ_MAX_TIME:-180}"
SANITIZER="${FUZZ_SANITIZER:-address}"
FUZZ_TARGETS="${FUZZ_TARGETS:-hpack_decode config_toml http2_frame_decode http_header_validate}"
REPORT="${RESULTS_DIR}/libfuzzer_asan_report.txt"

mkdir -p "${RESULTS_DIR}" "${CORPUS_DIR}"

if [[ "${SKIP_LIBFUZZER_ASAN:-1}" == "1" ]]; then
    echo "libfuzzer_asan: skipped (SKIP_LIBFUZZER_ASAN=1)" | tee "${REPORT}"
    exit 0
fi

# F-80: version-controlled な回帰 seed を永続コーパスへ複製（既存を上書きしない）。
# 既知クラッシュ（B-21 等）が必ず第 1 コーパスに含まれ、修正後の回帰を検出できる。
REGRESSION_CORPUS="${REPO_ROOT}/fuzz/regression_corpus"
if [[ "${SEED_REGRESSION_CORPUS:-1}" == "1" && -d "${REGRESSION_CORPUS}" ]]; then
    for tdir in "${REGRESSION_CORPUS}"/*/; do
        [[ -d "${tdir}" ]] || continue
        tname="$(basename "${tdir}")"
        mkdir -p "${CORPUS_DIR}/${tname}"
        cp -n "${tdir}"* "${CORPUS_DIR}/${tname}/" 2>/dev/null || true
    done
    echo "libfuzzer_asan: seeded regression corpus" | tee -a "${REPORT}"
fi

: >"${REPORT}"
echo "libfuzzer_asan start sanitizer=${SANITIZER} runs=${FUZZ_RUNS} max_time=${FUZZ_MAX_TIME}s" | tee -a "${REPORT}"

# corpus/ を /corpus にマウントし cargo fuzz の既定 corpus と同期（永続化）。
docker run --rm \
    -v "${REPO_ROOT}:/app:rw" \
    -v "${RESULTS_DIR}:/results:rw" \
    -v "${CORPUS_DIR}:/corpus:rw" \
    -w /app \
    -e CARGO_TARGET_DIR=/app/tools/container_security/results/cargo-target-asan \
    "${RUST_IMAGE}" \
    bash -c "
        set -euo pipefail
        rustup component add llvm-tools-preview rust-src 2>/dev/null || true
        cargo install cargo-fuzz --locked 2>/dev/null
        cd fuzz
        for target in ${FUZZ_TARGETS}; do
            mkdir -p /corpus/\${target}
            echo \"libfuzzer_asan target=\${target} sanitizer=${SANITIZER}\" | tee -a /results/libfuzzer_asan_report.txt
            # 永続 corpus を第1コーパスディレクトリとして渡す（発見シードが蓄積される）。
            cargo fuzz run --sanitizer ${SANITIZER} \"\${target}\" /corpus/\${target} -- \
                -runs=${FUZZ_RUNS} -max_total_time=${FUZZ_MAX_TIME} \
                2>&1 | tee -a /results/libfuzzer_asan_report.txt
        done
        echo 'libfuzzer_asan: ok' | tee -a /results/libfuzzer_asan_report.txt
    "

echo "libfuzzer_asan 完了: ${REPORT}（corpus: ${CORPUS_DIR}）" | tee -a "${REPORT}"
