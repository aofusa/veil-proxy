#!/usr/bin/env bash
# veil クロスプラットフォームバイナリ tar.gz/zip パッケージング（F-125）
#
# macOS（universal2-apple-darwin: x86_64 + aarch64 fat binary）を Docker
# （messense/cargo-zigbuild）で、Windows（x86_64-pc-windows-msvc / aarch64-pc-windows-msvc=aws_lc_rs）を Docker
# （messense/cargo-xwin）でクロスビルドし、単体バイナリ tar.gz/zip を
# packaging/output/ へ出力する。QEMU 実行・テストは行わない
# （クロスビルドが通ることのみを検証する。ユーザ指示: docs/artifacts/f125_windows_macos_design.md）。
#
# 使い方:
#   ./packaging/scripts/build-cross.sh --target macos
#   ./packaging/scripts/build-cross.sh --target windows
#
# 環境変数:
#   CARGO_FEATURES  ビルドする feature セット（デフォルト: "full"（http3, wasm 含む全機能））
#   XWIN_CACHE_DIR  windows ターゲットの xwin SDK キャッシュ（ホスト側ディレクトリ、
#                   デフォルト: ~/.xwin-cache）
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
PKG_ROOT="${ROOT}/packaging"
OUTPUT_DIR="${PKG_ROOT}/output"
BUILD_DIR="${PKG_ROOT}/build"
VERSION="$(awk -F'"' '/^version = / { print $2; exit }' "${ROOT}/Cargo.toml")"

# macOS / Windows クロスビルドデフォルト feature セット（full: http3, wasm 含む全機能。
# TLS 暗号は aws_lc_rs プロバイダを使用、quiche には BoringSSL を使用）。
DEFAULT_MACOS_FEATURES="full"

# Windows クロスビルドデフォルト feature セット（full: http3, wasm 含む全機能）。
DEFAULT_WINDOWS_FEATURES="full"

TARGET_OS=""

usage() {
    cat <<EOF
Usage: $(basename "$0") --target <macos|windows>

Build a standalone veil binary tarball/zip for a cross-compiled non-Linux
target using Docker: 'macos' (universal2-apple-darwin, via
messense/cargo-zigbuild) or 'windows' (x86_64=ring + aarch64=aws_lc_rs, via
messense/cargo-xwin).

Options:
  --target TARGET   Cross-build target: macos | windows (required)
  -h, --help        Show this help

Environment:
  CARGO_FEATURES    Cargo features to build with
                     (default: "${DEFAULT_MACOS_FEATURES}" for macos,
                      "${DEFAULT_WINDOWS_FEATURES}" for windows)
  XWIN_CACHE_DIR    Host directory used to cache the xwin Windows SDK
                     (windows target only; default: ~/.xwin-cache)

Output:
  packaging/output/veil-\${VERSION}-universal2-apple-darwin.tar.gz
  packaging/output/veil-\${VERSION}-{x86_64,aarch64}-pc-windows-msvc.zip
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --target) TARGET_OS="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
done

if [[ "${TARGET_OS}" != "macos" && "${TARGET_OS}" != "windows" ]]; then
    echo "ERROR: --target must be 'macos' or 'windows'" >&2
    usage >&2
    exit 1
fi

build_macos() {
    local features="${CARGO_FEATURES:-${DEFAULT_MACOS_FEATURES}}"
    local rust_target="universal2-apple-darwin"
    local archive_name="veil-${VERSION}-${rust_target}.tar.gz"

    echo "==> Building veil binary for ${rust_target} in Docker (messense/cargo-zigbuild)"
    echo "==> Features: ${features}"

    # macOS は rustls の暗号プロバイダに aws_lc_rs を使用する（Cargo.toml の target 別依存、F-131）。
    # aws-lc-sys は cmake をコンテナへ導入することでビルド可能。
    docker run --rm \
        -e CMAKE_SYSTEM_NAME="Darwin" \
        -e BORING_BSSL_NO_ASM=1 \
        -e AWS_LC_SYS_NO_PREFIX="" \
        -v "${ROOT}:/io" \
        -w /io \
        messense/cargo-zigbuild \
        bash -c "unset AWS_LC_SYS_NO_PREFIX; apt-get update -qq && apt-get install -y -qq cmake nasm yasm perl golang-go >/dev/null 2>&1; printf '#!/bin/sh\necho /opt/MacOSX11.3.sdk\n' > /usr/local/bin/xcrun && chmod +x /usr/local/bin/xcrun; cargo zigbuild --release --target ${rust_target} --no-default-features --features ${features}"

    local binary_path="${ROOT}/target/${rust_target}/release/veil"
    if [[ ! -f "${binary_path}" ]]; then
        echo "ERROR: expected binary not found: ${binary_path}" >&2
        exit 1
    fi

    # zigbuild が生成する universal2（fat）バイナリであることを確認（best-effort。
    # ホストに `file` が無い環境ではスキップする）。
    if command -v file >/dev/null 2>&1; then
        echo "==> file(1) output for ${binary_path}:"
        file "${binary_path}" || true
    fi

    mkdir -p "${OUTPUT_DIR}"
    local stage_parent="${BUILD_DIR}/tarball-${rust_target}"
    local dir_name="veil-${VERSION}-${rust_target}"
    rm -rf "${stage_parent}"
    mkdir -p "${stage_parent}/${dir_name}/www"

    install -m 0755 "${binary_path}" "${stage_parent}/${dir_name}/veil"
    install -m 0644 "${ROOT}/contrib/config/config.toml" "${stage_parent}/${dir_name}/config.toml.default"
    install -m 0644 "${ROOT}/docker/assets/www/index.html" "${stage_parent}/${dir_name}/www/index.html"

    cat > "${stage_parent}/${dir_name}/INSTALL.txt" <<EOF
veil ${VERSION} — ${rust_target}

universal2 バイナリ（x86_64 + aarch64 fat binary、cargo-zigbuild クロスビルド）。
QEMU 実機検証は行っていません（Docker クロスビルドが通ることのみ確認済み。
F-125: docs/artifacts/f125_windows_macos_design.md）。

インストール手順:

  install -m 0755 veil /usr/local/bin/veil
  mkdir -p /usr/local/etc/veil
  cp config.toml.default /usr/local/etc/veil/config.toml
  veil --config /usr/local/etc/veil/config.toml

macOS ネイティブのセキュリティ（sandbox_init/Seatbelt）:
  config.toml で [security] enable_sandbox_macos = true を設定すると、
  設定から導出した静的ファイルルート・TLS証明書/鍵・ログ/キャッシュディレクトリを
  基にした SBPL プロファイルを sandbox_init(3) で適用します（実機未検証のため
  保守的な最小プロファイル。ネットワーク・ファイル読み取りは無条件許可し、
  ファイル書き込みのみログ/キャッシュディレクトリへ限定します）。

含まれる feature: ${features}
（http3/wasm はこのビルドに含まれていません。F-125 のクロスビルド検証で
  aws-lc-sys/quiche/wasmtime の macOS クロス対応が未確認のため）
EOF

    tar -C "${stage_parent}" -czf "${OUTPUT_DIR}/${archive_name}" "${dir_name}"
    rm -rf "${stage_parent}"
    echo "==> Created ${OUTPUT_DIR}/${archive_name}"
}

# 1 つの Windows ターゲット（x86_64 または aarch64）をビルドして zip 化する。
# TLS 暗号プロバイダは x86_64 / aarch64 ともに aws_lc_rs を使用（F-131）。
# cmake + nasm をコンテナへ導入することで aws-lc-sys をビルドする。
_build_one_windows() {
    local rust_target="$1"
    local features="${CARGO_FEATURES:-${DEFAULT_WINDOWS_FEATURES}}"
    local archive_name="veil-${VERSION}-${rust_target}.zip"
    local xwin_cache="${XWIN_CACHE_DIR:-${HOME}/.xwin-cache}"
    local provider="aws_lc_rs"
    local setup='command -v cmake >/dev/null 2>&1 && command -v nasm >/dev/null 2>&1 || { apt-get update -qq && apt-get install -y -qq cmake nasm yasm perl golang-go >/dev/null 2>&1; }'

    echo "==> Building veil binary for ${rust_target} (provider=${provider}) in Docker (messense/cargo-xwin)"
    echo "==> Features: ${features}"
    mkdir -p "${xwin_cache}"

    # target/ をホストと共有する都合上、ホスト側の他の cargo ビルドと同時に走らせないこと
    # （AGENTS.md 検証手順: 1 つずつ実行。target 競合を避けるため）。
    docker run --rm \
        -e CMAKE_SYSTEM_NAME="Windows" \
        -e CC_x86_64_pc_windows_msvc="clang-cl" \
        -e CXX_x86_64_pc_windows_msvc="clang-cl" \
        -e AR_x86_64_pc_windows_msvc="llvm-ar" \
        -e CC_aarch64_pc_windows_msvc="clang-cl" \
        -e CXX_aarch64_pc_windows_msvc="clang-cl" \
        -e AR_aarch64_pc_windows_msvc="llvm-ar" \
        -e CFLAGS="-DOPENSSL_NO_ASM" \
        -e CXXFLAGS="-DOPENSSL_NO_ASM" \
        -e BORING_BSSL_NO_ASM=1 \
        -e AWS_LC_SYS_NO_PREFIX="" \
        -e XWIN_CACHE_DIR=/xwincache \
        -v "${xwin_cache}:/xwincache" \
        -v "${ROOT}:/io" \
        -w /io \
        messense/cargo-xwin \
        bash -c "unset AWS_LC_SYS_NO_PREFIX; ${setup}; cargo xwin build --release --target ${rust_target} --no-default-features --features ${features}"

    local binary_path="${ROOT}/target/${rust_target}/release/veil.exe"
    if [[ ! -f "${binary_path}" ]]; then
        echo "ERROR: expected binary not found: ${binary_path}" >&2
        exit 1
    fi

    mkdir -p "${OUTPUT_DIR}"
    local stage_parent="${BUILD_DIR}/zip-${rust_target}"
    local dir_name="veil-${VERSION}-${rust_target}"
    rm -rf "${stage_parent}"
    mkdir -p "${stage_parent}/${dir_name}/www"

    install -m 0755 "${binary_path}" "${stage_parent}/${dir_name}/veil.exe"
    install -m 0644 "${ROOT}/contrib/config/config.toml" "${stage_parent}/${dir_name}/config.toml.default"
    install -m 0644 "${ROOT}/docker/assets/www/index.html" "${stage_parent}/${dir_name}/www/index.html"

    cat > "${stage_parent}/${dir_name}/INSTALL.txt" <<EOF
veil ${VERSION} — ${rust_target}

Windows バイナリ（cargo-xwin クロスビルド、TLS プロバイダ=${provider}）。
QEMU/実機検証は行っていません（Docker クロスビルドが通ることのみ確認済み。
docs/artifacts/f125_windows_macos_design.md の Windows 節）。

インストール手順:

  1. veil.exe を任意のディレクトリへコピー
  2. config.toml.default を config.toml としてコピーし、必要に応じて編集
  3. veil.exe --config config.toml を実行

Windows ネイティブのセキュリティ（Job Object、best-effort）:
  config.toml で [security] enable_job_object_windows = true を設定すると、
  CreateJobObjectW + SetInformationJobObject でプロセスに最小限のリソース制限
  （ACTIVE_PROCESS=1、KILL_ON_JOB_CLOSE）を適用します。seccomp/Landlock相当の
  システムコールフィルタではなく、粗粒度のプロセス制限にとどまります
  （実機検証不可のため保守的な最小構成）。

含まれる feature: ${features}
（TLS 暗号は ${provider} プロバイダ、HTTP/3 には BoringSSL を使用します）
EOF

    (cd "${stage_parent}" && zip -r "${OUTPUT_DIR}/${archive_name}" "${dir_name}" >/dev/null)
    rm -rf "${stage_parent}"
    echo "==> Created ${OUTPUT_DIR}/${archive_name}"
}

build_windows() {
    # x86_64（ring）と aarch64（aws_lc_rs）の両方をビルドする。
    _build_one_windows x86_64-pc-windows-msvc
    _build_one_windows aarch64-pc-windows-msvc
}

case "${TARGET_OS}" in
    macos) build_macos ;;
    windows) build_windows ;;
esac
