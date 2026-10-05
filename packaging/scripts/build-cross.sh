#!/usr/bin/env bash
# veil クロスプラットフォームバイナリ tar.gz/zip パッケージング（F-125）
#
# 専用 Dockerfile（docker/Dockerfile.{macos,windows}）でクロスビルドし、
# 単体バイナリ tar.gz/zip を packaging/output/ へ出力する。
#
#   macos    universal2-apple-darwin（x86_64 + aarch64 fat binary） / cargo-zigbuild
#   windows  x86_64-pc-windows-msvc + aarch64-pc-windows-msvc      / cargo-xwin
#
# FreeBSD の Docker クロスビルドは B-49（aws-lc-sys の s2n-bignum asm が FreeBSD
# クロス構成で組み立てられずリンクに失敗する、未解決）により削除した
# （`docker/Dockerfile.freebsd` は撤去済み）。FreeBSD は
# QEMU VM 内ネイティブビルドが唯一の公式経路: `--target freebsd` を指定すると
# 案内を表示して終了する（下記 build_freebsd_unsupported 参照）。
#
# BSD 向けは `full` ではなく `full-freebsd`（システムアロケータ。B-89）を既定にする
# （tools/qemu/bsd-vm.sh freebsd <arch> build が使う）。OpenBSD（同じく VM
# ネイティブビルド）は `full-openbsd`（システムアロケータ + 同梱
# rustls(ring)/quiche(BoringSSL)）を使う。
#
# 各 Dockerfile は Dockerfile.glibc と同じ cacher/builder 2 段構成のため、
# ソース変更だけの再ビルドでは aws-lc-sys / boring-sys（quiche 内蔵 BoringSSL）の
# 重い C ビルドがレイヤキャッシュから再利用される（B-47）。
#
# macOS / Windows は QEMU 実行・実機検証を本スクリプトでは行わない
# （クロスビルドが通ることのみを検証する。docs/artifacts/f125_windows_macos_design.md）。
#
# 使い方:
#   ./packaging/scripts/build-cross.sh --target macos
#   ./packaging/scripts/build-cross.sh --target windows
#
# 環境変数:
#   CARGO_FEATURES  ビルドする feature セット（デフォルト: "full"（http3, wasm 含む全機能））
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
target using the dedicated, layer-cached Dockerfiles under docker/:
  macos    universal2-apple-darwin        (docker/Dockerfile.macos)
  windows  {x86_64,aarch64}-pc-windows-msvc (docker/Dockerfile.windows)

FreeBSD is NOT supported here (B-49: Docker cross build fails at link time
because aws-lc-sys assembles none of its s2n-bignum .S files under a
FreeBSD cross configuration). Pass --target freebsd to see the QEMU VM
native-build instructions.

Options:
  --target TARGET   Cross-build target: macos | windows (required)
  -h, --help        Show this help

Environment:
  CARGO_FEATURES    Cargo features to build with
                     (default: "${DEFAULT_MACOS_FEATURES}" for macos,
                      "${DEFAULT_WINDOWS_FEATURES}" for windows)

Output:
  packaging/output/veil-\${VERSION}-universal2-apple-darwin.tar.gz
  packaging/output/veil-\${VERSION}-{x86_64,aarch64}-pc-windows-msvc.zip
EOF
}

# FreeBSD の Docker クロスビルドは B-49（未解決）で削除済み。案内を表示して
# 非ゼロで終了する（黙って「不正な値」と言うだけにしない）。
build_freebsd_unsupported() {
    cat >&2 <<'EOF'
ERROR: FreeBSD の Docker クロスビルドは削除されました（B-49、未解決）。

  aws-lc-sys の s2n-bignum アセンブリが FreeBSD クロス構成で 1 つも
  組み立てられず、リンク段で `undefined symbol: curve25519_x25519_byte`
  などが多数発生するため、動かない経路として docker/Dockerfile.freebsd と
  この --target freebsd を撤去しました。詳細:
    docs/backlog/bugs/B-49-awslc-freebsd-cross-missing-s2n-bignum-asm.md

FreeBSD は QEMU VM 内ネイティブビルドが唯一の公式経路です:

  tools/qemu/bsd-vm.sh freebsd <arch> build   # <arch> = x86_64 | aarch64
  tools/qemu/bsd-vm.sh freebsd <arch> fetch   # → packaging/build/veil-freebsd-<arch>
  packaging/scripts/build-bsd.sh --os freebsd --arch <arch> --binary <上記のパス>

（`tools/qemu/bsd-vm.sh freebsd <arch> all` で setup〜fetch を一括実行できます）
EOF
    exit 1
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --target) TARGET_OS="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
done

if [[ "${TARGET_OS}" == "freebsd" ]]; then
    build_freebsd_unsupported
fi

if [[ "${TARGET_OS}" != "macos" && "${TARGET_OS}" != "windows" ]]; then
    echo "ERROR: --target must be 'macos' or 'windows'" >&2
    usage >&2
    exit 1
fi

# 専用 Dockerfile の `artifact` ステージをビルドし、成果物を dest_dir へ取り出す。
#
# buildkit のローカルエクスポータ（--output）を使わないのは、snap 版 Docker のように
# daemon がエクスポート先のファイル所有権を設定できない環境で
# `error setting metadata: lchownat: operation not permitted` になるため。
# イメージとして tag し `docker create` + `docker cp` で取り出す方式ならその制約を
# 受けない（コンテナは起動しない。artifact ステージには ENTRYPOINT だけ置いてある）。
#
#   $1 dockerfile  $2 rust_target  $3 features  $4 dest_dir  $5 binary name
_build_artifact() {
    local dockerfile="$1" rust_target="$2" features="$3" dest_dir="$4" bin_name="$5"
    local tag="veil-artifact:${rust_target}"

    docker build \
        -f "${dockerfile}" \
        --target artifact \
        --build-arg "RUST_TARGET=${rust_target}" \
        --build-arg "CARGO_FEATURES=${features}" \
        -t "${tag}" \
        "${ROOT}"

    rm -rf "${dest_dir}"
    mkdir -p "${dest_dir}"
    local cid
    cid="$(docker create "${tag}")"
    docker cp "${cid}:/${bin_name}" "${dest_dir}/${bin_name}"
    docker rm "${cid}" >/dev/null
}

build_macos() {
    local features="${CARGO_FEATURES:-${DEFAULT_MACOS_FEATURES}}"
    local rust_target="universal2-apple-darwin"
    local archive_name="veil-${VERSION}-${rust_target}.tar.gz"

    echo "==> Building veil binary for ${rust_target} via docker/Dockerfile.macos"
    echo "==> Features: ${features}"

    # docker/Dockerfile.macos は Dockerfile.glibc と同じ cacher/builder 2 段構成で、
    # aws-lc-sys / boring-sys の重い C ビルドをレイヤキャッシュに残す（B-47）。
    # AWS_LC_SYS_NO_PREFIX は .cargo/config.toml の [env] が唯一の設定箇所（macOS は "0"）。
    local artifact_dir="${BUILD_DIR}/artifact-${rust_target}"
    _build_artifact "${ROOT}/docker/Dockerfile.macos" "${rust_target}" \
        "${features}" "${artifact_dir}" veil

    local binary_path="${artifact_dir}/veil"
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
（TLS 暗号は aws_lc_rs プロバイダ、HTTP/3 (quiche) には内蔵 BoringSSL を使用します）
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
    local provider="aws_lc_rs"

    echo "==> Building veil binary for ${rust_target} (provider=${provider}) via docker/Dockerfile.windows"
    echo "==> Features: ${features}"

    # docker/Dockerfile.windows は Dockerfile.glibc と同じ cacher/builder 2 段構成で、
    # aws-lc-sys / boring-sys の重い C ビルドと xwin の Windows SDK 取得を
    # レイヤキャッシュに残す（B-47）。ホストの target/ は共有しないため、
    # ホスト側の cargo ビルドと同時に走らせても競合しない。
    # AWS_LC_SYS_NO_PREFIX は .cargo/config.toml の [env] が唯一の設定箇所（Windows は "0"）。
    local artifact_dir="${BUILD_DIR}/artifact-${rust_target}"
    _build_artifact "${ROOT}/docker/Dockerfile.windows" "${rust_target}" \
        "${features}" "${artifact_dir}" veil.exe

    local binary_path="${artifact_dir}/veil.exe"
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
