#!/usr/bin/env bash
# veil Linux パッケージ統合ビルド（.deb + .rpm + glibc/musl バイナリ tar.gz）
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
PKG_ROOT="${ROOT}/packaging"
DOCKER_DIR="${SCRIPT_DIR}/docker"
OUTPUT_DIR="${PKG_ROOT}/output"
BUILD_DIR="${PKG_ROOT}/build"
VERSION="$(awk -F'"' '/^version = / { print $2; exit }' "${ROOT}/Cargo.toml")"
ARCH="$(uname -m)"
USE_DOCKER=0
SKIP_BUILD=0
BINARY_PATH=""
BINARY_PATH_MUSL=""

usage() {
    cat <<EOF
Usage: $(basename "$0") [OPTIONS]

Build Debian/Ubuntu (.deb) and Amazon Linux 2023 (.rpm) packages,
plus standalone glibc/musl binary tarballs.

Options:
  --docker            Build binaries inside Docker (Dockerfile.glibc / Dockerfile.musl)
  --skip-build        Skip cargo/Docker binary builds; use --binary / --binary-musl
  --binary PATH       Pre-built glibc veil binary path (implies --skip-build)
  --binary-musl PATH  Pre-built musl veil binary path (optional with --skip-build)
  -h, --help          Show this help

Environment:
  CARGO_FEATURES      Cargo features (default: full)
  RUST_TARGET         glibc Rust target triple (default: x86_64-unknown-linux-gnu)
  RUST_TARGET_MUSL    musl Rust target triple (default: <RUST_TARGET with -gnu → -musl>)
  LIBC_VERSION        glibc suffix for zigbuild (default: .2.28)

Outputs:
  packaging/output/veil_\${VERSION}_<deb_arch>.deb
  packaging/output/veil-\${VERSION}-1.<rpm_arch>.rpm
  packaging/output/veil-\${VERSION}-<gnu_target>.tar.gz
  packaging/output/veil-\${VERSION}-<musl_target>.tar.gz
EOF
}

RPM_ONLY_INTERNAL=0
DEB_ONLY_INTERNAL=0
while [[ $# -gt 0 ]]; do
    case "$1" in
        --docker) USE_DOCKER=1; shift ;;
        --skip-build) SKIP_BUILD=1; shift ;;
        --binary) SKIP_BUILD=1; BINARY_PATH="$2"; shift 2 ;;
        --binary-musl) BINARY_PATH_MUSL="$2"; shift 2 ;;
        --rpm-only-internal) RPM_ONLY_INTERNAL=1; shift ;;
        --deb-only-internal) DEB_ONLY_INTERNAL=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
done

deb_arch() {
    case "${ARCH}" in
        x86_64) echo "amd64" ;;
        aarch64) echo "arm64" ;;
        *) echo "${ARCH}" ;;
    esac
}

rpm_arch() {
    case "${ARCH}" in
        x86_64) echo "x86_64" ;;
        aarch64) echo "aarch64" ;;
        *) echo "${ARCH}" ;;
    esac
}

# RUST_TARGET (gnu) から musl ターゲットを導出
default_musl_target() {
    local gnu="${RUST_TARGET:-x86_64-unknown-linux-gnu}"
    if [[ -n "${RUST_TARGET_MUSL:-}" ]]; then
        echo "${RUST_TARGET_MUSL}"
        return
    fi
    if [[ "${gnu}" == *"-linux-gnu" ]]; then
        echo "${gnu/%-linux-gnu/-linux-musl}"
    else
        echo "x86_64-unknown-linux-musl"
    fi
}

GNU_TARGET="${RUST_TARGET:-x86_64-unknown-linux-gnu}"
MUSL_TARGET="$(default_musl_target)"

# ARCH は既定でホスト（uname -m）だが、RUST_TARGET が指定されていればその
# アーキ接頭辞を優先する（クロスターゲット時に deb_arch/rpm_arch を正しく導出するため。
# 例: RUST_TARGET=aarch64-unknown-linux-gnu → ARCH=aarch64 → deb=arm64/rpm=aarch64）。
if [[ -n "${RUST_TARGET:-}" ]]; then
    ARCH="${RUST_TARGET%%-*}"
fi

DEB_ARCH="$(deb_arch)"
RPM_ARCH="$(rpm_arch)"
DEB_NAME="veil_${VERSION}_${DEB_ARCH}.deb"
RPM_NAME="veil-${VERSION}-1.${RPM_ARCH}.rpm"
TAR_GNU_NAME="veil-${VERSION}-${GNU_TARGET}.tar.gz"
TAR_MUSL_NAME="veil-${VERSION}-${MUSL_TARGET}.tar.gz"

build_binary_glibc_native() {
    echo "==> Building veil binary (glibc, features=${CARGO_FEATURES:-full})"
    cd "${ROOT}"
    local target="${GNU_TARGET}"
    local libc="${LIBC_VERSION:-.2.28}"
    local zig_target="${target}${libc}"

    if cargo zigbuild --help >/dev/null 2>&1; then
        echo "==> Using cargo zigbuild (${zig_target}) for broad glibc compatibility"
        cargo zigbuild --release --target "${zig_target}" --features "${CARGO_FEATURES:-full}" --locked
        BINARY_PATH="${ROOT}/target/${target}/release/veil"
    else
        echo "==> WARNING: cargo zigbuild unavailable; host glibc binary may not run on Amazon Linux 2023" >&2
        echo "==> Use ./packaging/scripts/build.sh --docker for portable packages" >&2
        cargo build --release --features "${CARGO_FEATURES:-full}" --locked
        BINARY_PATH="${ROOT}/target/release/veil"
    fi
}

build_binary_musl_native() {
    echo "==> Building veil binary (musl, features=${CARGO_FEATURES:-full})"
    cd "${ROOT}"
    local target="${MUSL_TARGET}"

    if cargo zigbuild --help >/dev/null 2>&1; then
        echo "==> Using cargo zigbuild (${target})"
        cargo zigbuild --release --target "${target}" --features "${CARGO_FEATURES:-full}" --locked
    else
        echo "==> Using cargo build --target ${target}"
        cargo build --release --target "${target}" --features "${CARGO_FEATURES:-full}" --locked
    fi
    BINARY_PATH_MUSL="${ROOT}/target/${target}/release/veil"
}

build_binary_glibc_docker() {
    echo "==> Building veil binary in Docker (glibc, features=${CARGO_FEATURES:-full})"
    local features="${CARGO_FEATURES:-full}"
    local target="${GNU_TARGET}"
    local libc="${LIBC_VERSION:-.2.28}"
    mkdir -p "${BUILD_DIR}"
    # aarch64 ターゲットは専用の cross Dockerfile（F-120 Phase 3）を使う。
    # ランタイムステージが arm64 イメージのため、docker build にも
    # --platform linux/arm64 を渡してメタデータを arm64 に揃える。
    local dockerfile="${ROOT}/docker/Dockerfile.glibc"
    local platform_arg=()
    if [[ "${ARCH}" == "aarch64" ]]; then
        dockerfile="${ROOT}/docker/Dockerfile.glibc.aarch64"
        platform_arg=(--platform linux/arm64)
    fi
    docker build "${platform_arg[@]}" -f "${dockerfile}" \
        --build-arg CARGO_FEATURES="${features}" \
        --build-arg RUST_TARGET="${target}" \
        --build-arg LIBC_VERSION="${libc}" \
        -t "veil:glibc-${ARCH}" \
        "${ROOT}"

    local cid
    cid=$(docker create "${platform_arg[@]}" "veil:glibc-${ARCH}")
    docker cp "${cid}:/veil" "${BUILD_DIR}/veil-glibc"
    docker rm "${cid}"

    chmod +x "${BUILD_DIR}/veil-glibc"
    BINARY_PATH="${BUILD_DIR}/veil-glibc"
}

build_binary_musl_docker() {
    echo "==> Building veil binary in Docker (musl, features=${CARGO_FEATURES:-full})"
    local features="${CARGO_FEATURES:-full}"
    local target="${MUSL_TARGET}"
    mkdir -p "${BUILD_DIR}"
    local dockerfile="${ROOT}/docker/Dockerfile.musl"
    local platform_arg=()
    if [[ "${ARCH}" == "aarch64" ]]; then
        dockerfile="${ROOT}/docker/Dockerfile.musl.aarch64"
        platform_arg=(--platform linux/arm64)
    fi
    docker build "${platform_arg[@]}" -f "${dockerfile}" \
        --build-arg CARGO_FEATURES="${features}" \
        --build-arg RUST_TARGET="${target}" \
        -t "veil:musl-${ARCH}" \
        "${ROOT}"

    local cid
    cid=$(docker create "${platform_arg[@]}" "veil:musl-${ARCH}")
    docker cp "${cid}:/veil" "${BUILD_DIR}/veil-musl"
    docker rm "${cid}"

    chmod +x "${BUILD_DIR}/veil-musl"
    BINARY_PATH_MUSL="${BUILD_DIR}/veil-musl"
}

# 単体バイナリを tar.gz にまとめ packaging/output へ配置
package_binary_tarball() {
    local binary_path="$1"
    local target_name="$2"
    local archive_name="$3"

    if [[ ! -f "${binary_path}" ]]; then
        echo "ERROR: binary not found for tarball: ${binary_path}" >&2
        exit 1
    fi

    mkdir -p "${OUTPUT_DIR}"
    local stage_parent="${BUILD_DIR}/tarball-${target_name}"
    local dir_name="veil-${VERSION}-${target_name}"
    rm -rf "${stage_parent}"
    mkdir -p "${stage_parent}/${dir_name}"
    install -m 0755 "${binary_path}" "${stage_parent}/${dir_name}/veil"
    tar -C "${stage_parent}" -czf "${OUTPUT_DIR}/${archive_name}" "${dir_name}"
    rm -rf "${stage_parent}"
    echo "==> Created ${OUTPUT_DIR}/${archive_name}"
}

package_binary_tarballs() {
    echo "==> Packaging standalone binary tarballs"
    package_binary_tarball "${BINARY_PATH}" "${GNU_TARGET}" "${TAR_GNU_NAME}"
    if [[ -n "${BINARY_PATH_MUSL}" && -f "${BINARY_PATH_MUSL}" ]]; then
        package_binary_tarball "${BINARY_PATH_MUSL}" "${MUSL_TARGET}" "${TAR_MUSL_NAME}"
    else
        echo "==> WARNING: musl binary unavailable; skipping ${TAR_MUSL_NAME}" >&2
    fi
}

fix_output_ownership() {
    if command -v docker >/dev/null 2>&1; then
        docker run --rm -v "${ROOT}:/src" alpine:3.20 \
            chown -R "$(id -u):$(id -g)" /src/packaging/output 2>/dev/null || true
    fi
}

if [[ "${USE_DOCKER}" -eq 1 && "${RPM_ONLY_INTERNAL}" -eq 0 && "${DEB_ONLY_INTERNAL}" -eq 0 ]]; then
    if [[ "${SKIP_BUILD}" -eq 0 ]]; then
        build_binary_glibc_docker
        build_binary_musl_docker
    elif [[ -z "${BINARY_PATH}" ]]; then
        BINARY_PATH="${ROOT}/target/release/veil"
    fi
    if [[ ! -f "${BINARY_PATH}" ]]; then
        echo "ERROR: binary not found: ${BINARY_PATH}" >&2
        exit 1
    fi

    package_binary_tarballs

    echo "==> Packaging in Docker (ubuntu:24.04)"
    rel_binary="${BINARY_PATH#"${ROOT}/"}"
    docker run --rm \
        -v "${ROOT}:/src" \
        -w /src \
        ubuntu:24.04 bash -c "
            set -euo pipefail
            export DEBIAN_FRONTEND=noninteractive
            apt-get update -qq
            apt-get install -y -qq dpkg-dev rpm
            ./packaging/scripts/build.sh --skip-build --binary /src/${rel_binary}
        "

    fix_output_ownership

    echo "==> All packages built in ${OUTPUT_DIR}/"
    ls -lh "${OUTPUT_DIR}/"
    exit 0
fi

if [[ "${SKIP_BUILD}" -eq 0 ]]; then
    build_binary_glibc_native
    build_binary_musl_native
elif [[ -z "${BINARY_PATH}" ]]; then
    BINARY_PATH="${ROOT}/target/release/veil"
fi

if [[ ! -f "${BINARY_PATH}" ]]; then
    echo "ERROR: binary not found: ${BINARY_PATH}" >&2
    exit 1
fi

mkdir -p "${OUTPUT_DIR}"

stage_rootfs() {
    local dest="$1"
    rm -rf "${dest}"
    mkdir -p "${dest}/usr/bin"
    mkdir -p "${dest}/usr/share/veil/www"
    mkdir -p "${dest}/usr/share/veil/scripts"
    mkdir -p "${dest}/lib/systemd/system"

    install -m 0755 "${BINARY_PATH}" "${dest}/usr/bin/veil"
    install -m 0644 "${ROOT}/contrib/config/config.toml" "${dest}/usr/share/veil/config.toml.default"
    install -m 0644 "${ROOT}/docker/assets/www/index.html" "${dest}/usr/share/veil/www/index.html"
    install -m 0644 "${ROOT}/contrib/systemd/veil.service" "${dest}/lib/systemd/system/veil.service"
    install -m 0755 "${SCRIPT_DIR}/postinstall.sh" "${dest}/usr/share/veil/scripts/postinstall.sh"
    install -m 0755 "${SCRIPT_DIR}/preuninstall.sh" "${dest}/usr/share/veil/scripts/preuninstall.sh"
}

build_deb() {
    echo "==> Building ${DEB_NAME}"
    if command -v dpkg-deb >/dev/null 2>&1; then
        local staging="${PKG_ROOT}/staging-deb"
        stage_rootfs "${staging}"

        mkdir -p "${staging}/DEBIAN"
        sed "s/^Version: .*/Version: ${VERSION}/" "${PKG_ROOT}/debian/DEBIAN/control" > "${staging}/DEBIAN/control"
        sed -i "s/^Architecture: .*/Architecture: ${DEB_ARCH}/" "${staging}/DEBIAN/control"
        install -m 0755 "${PKG_ROOT}/debian/DEBIAN/postinst" "${staging}/DEBIAN/postinst"
        install -m 0755 "${PKG_ROOT}/debian/DEBIAN/prerm" "${staging}/DEBIAN/prerm"

        dpkg-deb --build --root-owner-group "${staging}" "${OUTPUT_DIR}/${DEB_NAME}"
        echo "==> Created ${OUTPUT_DIR}/${DEB_NAME}"
    else
        echo "==> dpkg-deb not found; using Docker (ubuntu:24.04)"
        local rel_binary="${BINARY_PATH#"${ROOT}/"}"
        docker run --rm \
            -v "${ROOT}:/src" \
            -w /src \
            ubuntu:24.04 bash -c "
                set -euo pipefail
                export DEBIAN_FRONTEND=noninteractive
                apt-get update -qq
                apt-get install -y -qq dpkg-dev
                ./packaging/scripts/build.sh --skip-build --binary /src/${rel_binary} --deb-only-internal
            "
        if [[ -f "${OUTPUT_DIR}/${DEB_NAME}" ]]; then
            echo "==> Created ${OUTPUT_DIR}/${DEB_NAME}"
            return
        fi
        echo "ERROR: Docker DEB build failed" >&2
        exit 1
    fi
}

build_rpm_tree() {
    local rpm_top="${PKG_ROOT}/rpm"
    local rootfs="${rpm_top}/SOURCES/rootfs"
    stage_rootfs "${rootfs}"

    rm -rf "${rpm_top}/BUILD" "${rpm_top}/BUILDROOT" "${rpm_top}/RPMS" "${rpm_top}/SRPMS"
    mkdir -p "${rpm_top}/BUILD" "${rpm_top}/BUILDROOT" "${rpm_top}/RPMS" "${rpm_top}/SRPMS" "${rpm_top}/SPECS"
    cp "${PKG_ROOT}/rpm/veil.spec" "${rpm_top}/SPECS/veil.spec"

    rpmbuild -bb "${rpm_top}/SPECS/veil.spec" \
        --define "_topdir ${rpm_top}" \
        --define "_sourcedir ${rpm_top}/SOURCES" \
        --define "veil_version ${VERSION}" \
        --define "veil_arch ${RPM_ARCH}"
}

build_rpm() {
    echo "==> Building ${RPM_NAME}"

    if command -v rpmbuild >/dev/null 2>&1; then
        build_rpm_tree
    else
        echo "==> rpmbuild not found; using Docker (ubuntu:24.04)"
        local rel_binary="${BINARY_PATH#"${ROOT}/"}"
        docker run --rm \
            -v "${ROOT}:/src" \
            -w /src \
            ubuntu:24.04 bash -c "
                set -euo pipefail
                export DEBIAN_FRONTEND=noninteractive
                apt-get update -qq
                apt-get install -y -qq rpm
                ./packaging/scripts/build.sh --skip-build --binary /src/${rel_binary} --rpm-only-internal
            "
        if [[ -f "${OUTPUT_DIR}/${RPM_NAME}" ]]; then
            echo "==> Created ${OUTPUT_DIR}/${RPM_NAME}"
            return
        fi
        echo "ERROR: Docker RPM build failed" >&2
        exit 1
    fi

    local built
    built="$(find "${PKG_ROOT}/rpm/RPMS" -name "veil-${VERSION}-1.*.rpm" | head -1)"
    if [[ -z "${built}" ]]; then
        echo "ERROR: RPM build failed" >&2
        exit 1
    fi
    cp "${built}" "${OUTPUT_DIR}/${RPM_NAME}"
    echo "==> Created ${OUTPUT_DIR}/${RPM_NAME}"
}

if [[ "${DEB_ONLY_INTERNAL}" -eq 1 ]]; then
    build_deb
    exit 0
fi

if [[ "${RPM_ONLY_INTERNAL}" -eq 1 ]]; then
    build_rpm_tree
    built="$(find "${PKG_ROOT}/rpm/RPMS" -name "veil-${VERSION}-1.*.rpm" | head -1)"
    cp "${built}" "${OUTPUT_DIR}/${RPM_NAME}"
    echo "==> Created ${OUTPUT_DIR}/${RPM_NAME}"
    exit 0
fi

# フルビルド時、または musl バイナリも明示された --skip-build 時に tar.gz を生成。
# 内側 Docker の deb/rpm のみ経路（--skip-build --binary のみ）では再生成しない
# （外側 --docker 経路が先に package_binary_tarballs を呼ぶ）。
if [[ "${SKIP_BUILD}" -eq 0 || -n "${BINARY_PATH_MUSL}" ]]; then
    package_binary_tarballs
fi

build_deb
build_rpm

# Docker 内 rpmbuild が root 所有で出力する場合があるため所有者を修正
fix_output_ownership

echo "==> All packages built in ${OUTPUT_DIR}/"
ls -lh "${OUTPUT_DIR}/"
