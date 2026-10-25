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

# F-264: ライブラリが同梱する Swagger UI（third_party/swagger-ui-dist。crate ではないので cargo-audit は見ない）を
# npm の advisory で照合する。該当があれば直す版の有無にかかわらず失敗（判定マーカーを書く前に止める）
"${REPO_ROOT}/tools/release/update-swagger-ui.sh" --audit 2>&1 | tee -a "${REPORT}"

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
        # B-158: 保守終了（unmaintained）・unsound・yanked の警告も失敗にする（黙って残さない。やむを得ず残すものは
        # .cargo/audit.toml の ignore に理由付きで書く）
        if ! command -v cargo-audit >/dev/null 2>&1; then
            cargo install cargo-audit --locked --quiet
        fi
        timeout 300 cargo audit --color never --deny warnings 2>&1 | tee /results/cargo_audit_report.txt
        # 本体以外の Cargo.lock（例・試験用のクレート・計測ツール・VDS の参照サーバ）も見る（B-158。本体だけを
        # 見ていたので、例の rustls-webpki などが修正前のまま残っていた）
        for lock in $(find . -name target -prune -o -path ./third_party -prune -o -name Cargo.lock -print | sort); do
            [ "$lock" = ./Cargo.lock ] && continue
            echo "== $lock" | tee -a /results/cargo_audit_report.txt
            timeout 300 cargo audit --color never --deny warnings -f "$lock" 2>&1 | tee -a /results/cargo_audit_report.txt
        done
        # 判定マーカーはレポートファイルへ書く（lib/report.sh がファイルを grep する）。
        echo "cargo_audit: ok" | tee -a /results/cargo_audit_report.txt
    '