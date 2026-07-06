#!/usr/bin/env bash
# veil Linux (.deb) パッケージビルドスクリプト
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PKG_DIR="${ROOT}/packaging/debian"
STAGING="${ROOT}/packaging/staging"
OUTPUT_DIR="${ROOT}/packaging/output"
VERSION="$(awk -F'"' '/^version = / { print $2; exit }' "${ROOT}/Cargo.toml")"
ARCH="$(uname -m)"
DEB_ARCH="${ARCH}"
case "${ARCH}" in
    x86_64) DEB_ARCH="amd64" ;;
    aarch64) DEB_ARCH="arm64" ;;
esac

DEB_NAME="veil_${VERSION}_${DEB_ARCH}.deb"

echo "==> Building veil binary (features=full)"
cd "${ROOT}"
cargo build --release --features full --locked

echo "==> Assembling package tree"
rm -rf "${STAGING}"
mkdir -p "${STAGING}/DEBIAN"
mkdir -p "${STAGING}/usr/bin"
mkdir -p "${STAGING}/usr/share/veil/www"
mkdir -p "${STAGING}/lib/systemd/system"
mkdir -p "${STAGING}/var/log/veil"
mkdir -p "${STAGING}/var/cache/veil"
mkdir -p "${STAGING}/var/tmp/veil"
mkdir -p "${STAGING}/var/etc/veil/ssl"
touch "${STAGING}/var/log/veil/.keep"
touch "${STAGING}/var/cache/veil/.keep"
touch "${STAGING}/var/tmp/veil/.keep"

install -m 0755 "${ROOT}/target/release/veil" "${STAGING}/usr/bin/veil"
install -m 0644 "${ROOT}/contrib/config/config.toml" "${STAGING}/usr/share/veil/config.toml.default"
install -m 0644 "${ROOT}/docker/assets/www/index.html" "${STAGING}/usr/share/veil/www/index.html"
install -m 0644 "${ROOT}/contrib/systemd/veil.service" "${STAGING}/lib/systemd/system/veil.service"

sed "s/^Version: .*/Version: ${VERSION}/" "${PKG_DIR}/DEBIAN/control" > "${STAGING}/DEBIAN/control"
sed -i "s/^Architecture: .*/Architecture: ${DEB_ARCH}/" "${STAGING}/DEBIAN/control"
install -m 0755 "${PKG_DIR}/DEBIAN/postinst" "${STAGING}/DEBIAN/postinst"
install -m 0755 "${PKG_DIR}/DEBIAN/prerm" "${STAGING}/DEBIAN/prerm"

mkdir -p "${OUTPUT_DIR}"
echo "==> Building ${DEB_NAME}"
dpkg-deb --build --root-owner-group "${STAGING}" "${OUTPUT_DIR}/${DEB_NAME}"
echo "==> Package created: ${OUTPUT_DIR}/${DEB_NAME}"