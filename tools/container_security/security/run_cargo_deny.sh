#!/usr/bin/env bash
# cargo-deny による依存関係・ライセンス検証（Rust コンテナ内完結）
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
RESULTS_DIR="${RESULTS_DIR:-${REPO_ROOT}/tools/container_security/results}"
RUST_IMAGE="${RUST_DENY_IMAGE:-rust:1-bookworm}"
REPORT="${RESULTS_DIR}/cargo_deny_report.txt"

mkdir -p "${RESULTS_DIR}"

if [[ "${SKIP_CARGO_DENY:-1}" == "1" ]]; then
    echo "cargo_deny: skipped (SKIP_CARGO_DENY=1)" | tee "${REPORT}"
    exit 0
fi

echo "cargo_deny start" | tee "${REPORT}"

docker run --rm \
    -v "${REPO_ROOT}:/app:ro" \
    -v "${RESULTS_DIR}:/results:rw" \
    -v veil-sec-cargo-deny-cache:/usr/local/cargo/registry \
    -v veil-sec-cargo-deny-git:/usr/local/cargo/git \
    -v veil-sec-cargo-deny-bin:/usr/local/cargo/bin \
    -w /app \
    "${RUST_IMAGE}" \
    bash -c '
        set -euo pipefail
        if ! command -v cargo-deny >/dev/null 2>&1; then
            cargo install cargo-deny --locked --quiet
        fi
        timeout 300 cargo deny check advisories licenses --color never 2>&1 \
            | tee /results/cargo_deny_report.txt
        echo "cargo_deny: ok"
    '