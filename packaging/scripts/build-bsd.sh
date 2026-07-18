#!/usr/bin/env bash
# veil FreeBSD/OpenBSD バイナリ tar.gz パッケージング（F-120 Phase 6）
#
# FreeBSD / OpenBSD は Rust Tier 2/3 かつクロスビルド困難なため、バイナリは
# QEMU VM 内でネイティブビルドしたものを --binary で受け取り、rc.d サービス
# スクリプト・設定リファレンス・（FreeBSD は）jail.conf サンプルを同梱した
# tar.gz を packaging/output/ へ出力する。deb/rpm は Linux 専用のため BSD では
# tar.gz のみ（BSD ネイティブの pkg 形式化は将来課題）。
#
# 使い方（VM でビルドしたバイナリを host へ持ち出してから）:
#   ./packaging/scripts/build-bsd.sh --os freebsd --arch x86_64 --binary ./veil-freebsd-amd64
#   ./packaging/scripts/build-bsd.sh --os openbsd --arch x86_64 --binary ./veil-openbsd-amd64
#
# ターゲットトリプル命名（tar.gz 名）:
#   freebsd: <arch>-unknown-freebsd    openbsd: <arch>-unknown-openbsd
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
PKG_ROOT="${ROOT}/packaging"
OUTPUT_DIR="${PKG_ROOT}/output"
BUILD_DIR="${PKG_ROOT}/build"
BSD_ASSETS="${PKG_ROOT}/bsd"
VERSION="$(awk -F'"' '/^version = / { print $2; exit }' "${ROOT}/Cargo.toml")"

OS=""
ARCH="x86_64"
BINARY=""

usage() {
    cat <<EOF
Usage: $(basename "$0") --os {freebsd|openbsd} [--arch {x86_64|aarch64}] --binary PATH [--os-version VER]

Assemble a FreeBSD/OpenBSD binary tarball with rc.d service script,
config reference, and (FreeBSD) jail.conf sample. The OS version the binary
was built on is recorded in BUILD_INFO.txt / INSTALL.txt.

Options:
  --os OS            Target OS: freebsd or openbsd (required)
  --arch ARCH        Target arch: x86_64 (default) or aarch64
  --binary PATH      Pre-built veil binary for the target OS/arch (required;
                     build it inside a matching QEMU VM)
  --os-version VER   OS release the binary was built on (e.g. 14.3-RELEASE,
                     7.6). Auto-detected via 'uname -r' when run on the target
                     OS; specify explicitly otherwise.
  -h, --help         Show this help

Output:
  packaging/output/veil-\${VERSION}-<arch>-unknown-<os>.tar.gz
EOF
}

OS_VERSION=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --os) OS="$2"; shift 2 ;;
        --arch) ARCH="$2"; shift 2 ;;
        --binary) BINARY="$2"; shift 2 ;;
        --os-version) OS_VERSION="$2"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
done

if [[ "${OS}" != "freebsd" && "${OS}" != "openbsd" ]]; then
    echo "ERROR: --os must be freebsd or openbsd" >&2; usage >&2; exit 1
fi

# ビルドした OS のバージョンを明記する（ユーザ要件）。build-bsd.sh は対象 OS の VM 内で
# ネイティブビルドした後に実行する想定のため、未指定なら `uname` から自動検出する
# （例: FreeBSD 14.3-RELEASE / OpenBSD 7.6）。対象 OS 上で実行していない場合は
# --os-version で明示する（未指定かつ検出不可なら unknown として警告）。
if [[ -z "${OS_VERSION}" ]]; then
    host_os="$(uname -s 2>/dev/null | tr '[:upper:]' '[:lower:]')"
    if [[ "${host_os}" == "${OS}" ]]; then
        OS_VERSION="$(uname -r 2>/dev/null || echo unknown)"
    else
        OS_VERSION="unknown"
        echo "WARNING: --os-version 未指定かつ ${OS} 上で実行していないため OS バージョンを" >&2
        echo "         検出できません。'--os-version 14.3-RELEASE' のように明示してください。" >&2
    fi
fi
if [[ -z "${BINARY}" || ! -f "${BINARY}" ]]; then
    echo "ERROR: --binary PATH must point to a pre-built ${OS} binary" >&2; exit 1
fi
if [[ "${ARCH}" != "x86_64" && "${ARCH}" != "aarch64" ]]; then
    echo "ERROR: --arch must be x86_64 or aarch64" >&2; exit 1
fi

TARGET="${ARCH}-unknown-${OS}"
ARCHIVE_NAME="veil-${VERSION}-${TARGET}.tar.gz"

mkdir -p "${OUTPUT_DIR}"
stage_parent="${BUILD_DIR}/tarball-${TARGET}"
dir_name="veil-${VERSION}-${TARGET}"
rm -rf "${stage_parent}"
mkdir -p "${stage_parent}/${dir_name}"

# バイナリ
install -m 0755 "${BINARY}" "${stage_parent}/${dir_name}/veil"

# 設定リファレンス・静的コンテンツ
install -m 0644 "${ROOT}/contrib/config/config.toml" "${stage_parent}/${dir_name}/config.toml.default"
install -m 0644 "${ROOT}/docker/assets/www/index.html" "${stage_parent}/${dir_name}/www/index.html" 2>/dev/null || {
    mkdir -p "${stage_parent}/${dir_name}/www"
    install -m 0644 "${ROOT}/docker/assets/www/index.html" "${stage_parent}/${dir_name}/www/index.html"
}

# rc.d サービススクリプト（+ FreeBSD は jail.conf サンプル）
if [[ "${OS}" == "freebsd" ]]; then
    install -m 0555 "${BSD_ASSETS}/freebsd/veil.rc" "${stage_parent}/${dir_name}/rc.d/veil" 2>/dev/null || {
        mkdir -p "${stage_parent}/${dir_name}/rc.d"
        install -m 0555 "${BSD_ASSETS}/freebsd/veil.rc" "${stage_parent}/${dir_name}/rc.d/veil"
    }
    install -m 0644 "${BSD_ASSETS}/freebsd/jail.conf.sample" "${stage_parent}/${dir_name}/jail.conf.sample"
else
    mkdir -p "${stage_parent}/${dir_name}/rc.d"
    install -m 0555 "${BSD_ASSETS}/openbsd/veil.rc" "${stage_parent}/${dir_name}/rc.d/veil"
fi

# ビルド情報（ビルドした OS バージョンを明記）
cat > "${stage_parent}/${dir_name}/BUILD_INFO.txt" <<EOF
veil ${VERSION}
target      : ${TARGET}
built on OS : ${OS} ${OS_VERSION}
built at    : $(date -u '+%Y-%m-%dT%H:%M:%SZ')
rustc       : $(rustc --version 2>/dev/null || echo unknown)

このバイナリは ${OS} ${OS_VERSION} 上でネイティブビルドされたものです。
同一メジャーバージョン系列での動作を想定しています（ABI 互換のため、大きく異なる
${OS} バージョンでは再ビルドを推奨します）。
EOF

# インストール手順 README
cat > "${stage_parent}/${dir_name}/INSTALL.txt" <<EOF
veil ${VERSION} — ${TARGET}
ビルド OS: ${OS} ${OS_VERSION}（詳細は BUILD_INFO.txt）

インストール手順（root で実行）:

  # バイナリ
  install -m 0755 veil /usr/local/bin/veil

  # 設定（初回のみ）
EOF
if [[ "${OS}" == "freebsd" ]]; then
    cat >> "${stage_parent}/${dir_name}/INSTALL.txt" <<EOF
  mkdir -p /usr/local/etc/veil
  cp config.toml.default /usr/local/etc/veil/config.toml

  # rc.d サービス
  install -m 0555 rc.d/veil /usr/local/etc/rc.d/veil
  sysrc veil_enable=YES
  service veil start

  # （任意）jail 内で稼働させる場合は jail.conf.sample を参照
  # veil の [security] enable_capsicum = true で capsicum 併用を推奨
EOF
else
    cat >> "${stage_parent}/${dir_name}/INSTALL.txt" <<EOF
  mkdir -p /etc/veil
  cp config.toml.default /etc/veil/config.toml

  # rc.d サービス
  install -m 0555 rc.d/veil /etc/rc.d/veil
  rcctl enable veil
  rcctl start veil

  # OpenBSD ネイティブのセキュリティ:
  #   config.toml で enable_pledge = true / enable_unveil = true を設定
  # TLS は rustls の ring プロバイダで動作（F-122）。HTTPS 静的配信/プロキシとも
  #   pledge+unveil 有効のまま 200 で動作することを検証済み。
EOF
fi

tar -C "${stage_parent}" -czf "${OUTPUT_DIR}/${ARCHIVE_NAME}" "${dir_name}"
rm -rf "${stage_parent}"
echo "==> Created ${OUTPUT_DIR}/${ARCHIVE_NAME} (built on ${OS} ${OS_VERSION})"
