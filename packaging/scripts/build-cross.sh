#!/usr/bin/env bash
# veil クロスプラットフォームバイナリ tar.gz パッケージング（F-125）
#
# macOS（universal2-apple-darwin: x86_64 + aarch64 fat binary）を Docker
# （messense/cargo-zigbuild）でクロスビルドし、単体バイナリ tar.gz を
# packaging/output/ へ出力する。QEMU 実行・テストは行わない
# （クロスビルドが通ることのみを検証する。ユーザ指示: docs/artifacts/f125_windows_macos_design.md）。
#
# 使い方:
#   ./packaging/scripts/build-cross.sh --target macos
#
# 環境変数:
#   CARGO_FEATURES  ビルドする feature セット
#                   （デフォルト: F-125 でクロスビルドが通ることを確認済みの最大セット。
#                    http3/wasm は quiche/wasmtime の macOS クロス対応が未検証のため
#                    既定では含めない。macOS の TLS 暗号は ring プロバイダを使う）
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
PKG_ROOT="${ROOT}/packaging"
OUTPUT_DIR="${PKG_ROOT}/output"
BUILD_DIR="${PKG_ROOT}/build"
VERSION="$(awk -F'"' '/^version = / { print $2; exit }' "${ROOT}/Cargo.toml")"

# F-125 で確認済みの macOS クロスビルド最大 feature セット（http3/wasm 除く。
# 設計 docs/artifacts/f125_windows_macos_design.md 参照）。
DEFAULT_MACOS_FEATURES="http2,mimalloc,compression,cache,metrics,websocket,rate-limit,buffering,admin,access-log,l4-proxy"

TARGET_OS=""

usage() {
    cat <<EOF
Usage: $(basename "$0") --target macos

Build a standalone veil binary tarball for a cross-compiled non-Linux
target using Docker. Currently only 'macos' (universal2-apple-darwin,
via messense/cargo-zigbuild) is supported (F-125). Windows cross-build
(cargo-xwin) is a follow-up (F-125 tracks macOS only for the runtime
implementation; Windows currently only gets build.rs cfg names).

Options:
  --target TARGET   Cross-build target: macos (required)
  -h, --help        Show this help

Environment:
  CARGO_FEATURES    Cargo features to build with
                     (default: "${DEFAULT_MACOS_FEATURES}")

Output:
  packaging/output/veil-\${VERSION}-universal2-apple-darwin.tar.gz
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --target) TARGET_OS="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
done

if [[ "${TARGET_OS}" != "macos" ]]; then
    echo "ERROR: --target must be 'macos' (Windows cross-build is a follow-up)" >&2
    usage >&2
    exit 1
fi

build_macos() {
    local features="${CARGO_FEATURES:-${DEFAULT_MACOS_FEATURES}}"
    local rust_target="universal2-apple-darwin"
    local archive_name="veil-${VERSION}-${rust_target}.tar.gz"

    echo "==> Building veil binary for ${rust_target} in Docker (messense/cargo-zigbuild)"
    echo "==> Features: ${features}"

    # macOS は rustls の暗号プロバイダに ring を使う（Cargo.toml の target 別依存、F-125）。
    # aws-lc-sys の手書きアセンブリ .S.o を zig リンカが解釈できずリンク失敗し
    # （unknown cpu architecture）、release では AWS_LC_SYS_NO_ASM も禁止されるため、
    # apple-darwin クロスビルド実績のある ring に切り替えている（src/tls_provider.rs 参照）。
    # ring は cc ベースでビルドでき、aws-lc-sys の cmake は不要。
    # target/ をホストと共有する都合上、ホスト側の他の cargo ビルドと同時に走らせないこと
    # （AGENTS.md 検証手順: 1 つずつ実行。target 競合を避けるため）。
    docker run --rm \
        -v "${ROOT}:/io" \
        -w /io \
        messense/cargo-zigbuild \
        cargo zigbuild --release --target "${rust_target}" --features "${features}"

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

build_macos
