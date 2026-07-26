#!/usr/bin/env bash
#
# Linux aarch64: Docker クロスビルド済みバイナリ + QEMU VM 上の tests/e2e_setup.sh（B-47）
# ============================================================================
#
# `run-e2e-aarch64.sh` は HTTPS 静的配信の**スモーク**だけを VM 内で実行する。
# 本スクリプトはより強い検証として **tests/e2e_setup.sh test の全 E2E スイート**を
# 実 aarch64 カーネル（io_uring）上で実行する。
#
# 流れ:
#   1. docker/Dockerfile.glibc.aarch64 で veil を aarch64 向けにビルドし、
#      成果物バイナリだけをホストへ取り出す（VM 内で veil をビルドしない）。
#   2. tools/qemu/aarch64-vm.sh の VM を起動して待つ。
#   3. VM へリポジトリと veil バイナリを転送し、Rust ツールチェーンを用意する。
#   4. VM 内で `VEIL_E2E_SKIP_VEIL_BUILD=1 tests/e2e_setup.sh test` を実行する。
#      （veil 本体はクロスビルド済みを使うが、grpc_server / test_backends /
#        E2E テストバイナリ自体は cargo が必要なので VM 内でビルドされる。）
#
# **重要な前提**: x86_64 ホスト上の aarch64 full-system は TCG（ソフトウェア
# エミュレーション）で動くため非常に低速で、KVM が使えないホストでは
# `tools/qemu/README.md` の「既知の環境制約」のとおり Ubuntu/Alpine cloud image が
# 起動途中で実用不能なほど遅くなることがある。ネイティブ aarch64 ホストや
# ネスト仮想化で KVM が使える環境でのみ現実的に完走する。
#
# 使い方:
#   tools/qemu/linux-aarch64-e2e.sh              # ビルド → VM 起動 → E2E
#   SKIP_BUILD=1 tools/qemu/linux-aarch64-e2e.sh # 既存の成果物を使う
#
# 環境変数:
#   CARGO_FEATURES  ビルド/E2E の feature（既定 full）
#   SKIP_BUILD      1 で Docker クロスビルドを省略
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
VM="${SCRIPT_DIR}/aarch64-vm.sh"
RUST_TARGET="aarch64-unknown-linux-gnu"
CARGO_FEATURES="${CARGO_FEATURES:-full}"
STAGE="${ROOT}/target/aarch64-e2e-artifact"
GUEST_ROOT="/home/veil/veil-proxy"

log() { echo "[linux-aarch64-e2e] $*" >&2; }
die() { echo "[linux-aarch64-e2e] ERROR: $*" >&2; exit 1; }

# 1) Docker で aarch64 バイナリをビルドして取り出す
if [[ "${SKIP_BUILD:-0}" != "1" ]]; then
    log "docker/Dockerfile.glibc.aarch64 で ${RUST_TARGET} 向けにビルド"
    rm -rf "${STAGE}"; mkdir -p "${STAGE}"
    docker build \
        -f "${ROOT}/docker/Dockerfile.glibc.aarch64" \
        --platform linux/arm64 \
        --build-arg "CARGO_FEATURES=${CARGO_FEATURES}" \
        -t veil:glibc-aarch64 \
        "${ROOT}"
    # runtime ステージのイメージから veil を取り出す
    cid="$(docker create --platform linux/arm64 veil:glibc-aarch64)"
    docker cp "${cid}:/veil" "${STAGE}/veil"
    docker rm "${cid}" >/dev/null
fi
BIN="${STAGE}/veil"
[[ -f "${BIN}" ]] || die "aarch64 バイナリが見つかりません: ${BIN}（SKIP_BUILD=1 のまま実行した？）"

# 2) VM 起動
log "aarch64 VM を起動して SSH 到達を待つ（TCG では非常に時間がかかる）"
"${VM}" up || true
"${VM}" wait

# 3) 転送 + ツールチェーン
log "VM へリポジトリを転送"
"${VM}" ssh "rm -rf ${GUEST_ROOT} && mkdir -p ${GUEST_ROOT}"
(cd "${ROOT}" && tar czf - \
    src benches tests examples contrib docker/assets \
    Cargo.toml Cargo.lock build.rs clippy.toml .cargo) \
  | "${VM}" ssh "cd ${GUEST_ROOT} && tar xzf - && sed -i 's|members = \[\".\", \"fuzz\"\]|members = [\".\"]|' Cargo.toml"

log "クロスビルド済み veil を配置（VM 内では veil をビルドしない）"
"${VM}" ssh "mkdir -p ${GUEST_ROOT}/target/debug"
"${VM}" scp "${BIN}" "${GUEST_ROOT}/target/debug/veil"
"${VM}" ssh "chmod +x ${GUEST_ROOT}/target/debug/veil && ${GUEST_ROOT}/target/debug/veil --version || true"

log "VM 内の Rust ツールチェーン / E2E 依存を用意"
"${VM}" ssh 'command -v cargo >/dev/null 2>&1 || (sudo apt-get update -qq && sudo apt-get install -y -qq cargo rustc build-essential pkg-config libssl-dev cmake nasm curl openssl)'

# 4) E2E 実行
log "VM 内で tests/e2e_setup.sh test を実行（features=${CARGO_FEATURES}）"
"${VM}" ssh "cd ${GUEST_ROOT} && VEIL_E2E_SKIP_VEIL_BUILD=1 VEIL_E2E_FEATURES='${CARGO_FEATURES}' bash tests/e2e_setup.sh test"

log "完了"
