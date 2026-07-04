#!/usr/bin/env bash
# cargo-audit による依存関係脆弱性スキャン（Rust コンテナ内完結）
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
RESULTS_DIR="${RESULTS_DIR:-${REPO_ROOT}/tools/container_security/results}"
RUST_IMAGE="${RUST_AUDIT_IMAGE:-rust:1-bookworm}"
REPORT="${RESULTS_DIR}/cargo_audit_report.txt"

mkdir -p "${RESULTS_DIR}"

echo "cargo_audit start" | tee "${REPORT}"

docker run --rm \
    -v "${REPO_ROOT}:/app:ro" \
    -v "${RESULTS_DIR}:/results:rw" \
    -v veil-sec-cargo-audit-cache:/usr/local/cargo/registry \
    -v veil-sec-cargo-audit-git:/usr/local/cargo/git \
    -v veil-sec-cargo-audit-bin:/usr/local/cargo/bin \
    -w /app \
    "${RUST_IMAGE}" \
    bash -c '
        set -euo pipefail
        if ! command -v cargo-audit >/dev/null 2>&1; then
            cargo install cargo-audit --locked --quiet
        fi
        timeout 300 cargo audit --color never 2>&1 | tee /results/cargo_audit_report.txt
        echo "cargo_audit: ok"
    '