#!/usr/bin/env bash
# veil FreeBSD/OpenBSD/NetBSD バイナリ tar.gz パッケージング（F-120 Phase 6 / F-140）
#
# FreeBSD / OpenBSD / NetBSD は Rust Tier 2/3 かつクロスビルド困難なため、バイナリは
# QEMU VM 内でネイティブビルドしたものを --binary で受け取り、rc.d サービス
# スクリプト・設定リファレンス・（FreeBSD は）jail.conf サンプルを同梱した
# tar.gz を packaging/output/ へ出力する。deb/rpm は Linux 専用のため BSD では
# tar.gz のみ（BSD ネイティブの pkg 形式化は将来課題）。
#
# 使い方（VM でビルドしたバイナリを host へ持ち出してから）:
#   ./packaging/scripts/build-bsd.sh --os freebsd --arch x86_64 --binary ./veil-freebsd-amd64
#   ./packaging/scripts/build-bsd.sh --os openbsd --arch x86_64 --binary ./veil-openbsd-amd64
#   ./packaging/scripts/build-bsd.sh --os netbsd  --arch x86_64 --binary ./veil-netbsd-amd64
#
# tools/qemu/bsd-vm.sh で取得したバイナリをそのまま使う場合（推奨）:
#   tools/qemu/bsd-vm.sh freebsd x86_64 fetch      # → packaging/build/veil-freebsd-x86_64
#   ./packaging/scripts/build-bsd.sh --os freebsd --arch x86_64 --from-qemu
#   ./packaging/scripts/build-bsd.sh --all         # 取得済みの 6 通りをまとめて
#
# ターゲットトリプル命名（tar.gz 名）:
#   freebsd: <arch>-unknown-freebsd    openbsd: <arch>-unknown-openbsd
#   netbsd:  <arch>-unknown-netbsd
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
FROM_QEMU=0
ALL=0

usage() {
    cat <<EOF
Usage: $(basename "$0") --os {freebsd|openbsd|netbsd} [--arch {x86_64|aarch64}] --binary PATH [--os-version VER]
       $(basename "$0") --os {freebsd|openbsd|netbsd} --arch {x86_64|aarch64} --from-qemu
       $(basename "$0") --all

Assemble a FreeBSD/OpenBSD/NetBSD binary tarball with rc.d service script,
config reference, and (FreeBSD) jail.conf sample. The OS version the binary
was built on is recorded in BUILD_INFO.txt / INSTALL.txt.

Options:
  --os OS            Target OS: freebsd, openbsd, or netbsd (required)
  --arch ARCH        Target arch: x86_64 (default) or aarch64
  --binary PATH      Pre-built veil binary for the target OS/arch (required;
                     build it inside a matching QEMU VM)
  --os-version VER   OS release the binary was built on (e.g. 14.3-RELEASE,
                     7.9, 10.1). Auto-detected via 'uname -r' when run on the
                     target OS; specify explicitly otherwise.
  --from-qemu        Use the binary fetched by
                     'tools/qemu/bsd-vm.sh <os> <arch> fetch', i.e.
                     packaging/build/veil-<os>-<arch> together with the OS
                     version recorded in the matching .os-version file.
                     (--binary / --os-version are then unnecessary.)
  --all              Package every fetched BSD binary found under
                     packaging/build/ (freebsd/openbsd/netbsd x x86_64/aarch64).
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
        --from-qemu) FROM_QEMU=1; shift ;;
        --all) ALL=1; FROM_QEMU=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
done

# --all: packaging/build/ にある取得済みバイナリを総当たりでパッケージ化する。
# 取得は tools/qemu/bsd-vm.sh <os> <arch> fetch が行う。
if (( ALL )); then
    found=0
    for os_name in freebsd openbsd netbsd; do
        for arch_name in x86_64 aarch64; do
            bin="${BUILD_DIR}/veil-${os_name}-${arch_name}"
            [[ -f "${bin}" ]] || continue
            found=1
            ver="unknown"
            [[ -f "${bin}.os-version" ]] && ver="$(cat "${bin}.os-version")"
            echo "==> ${os_name}/${arch_name} (built on ${ver})"
            "$0" --os "${os_name}" --arch "${arch_name}" --binary "${bin}" --os-version "${ver}"
        done
    done
    if (( ! found )); then
        echo "ERROR: packaging/build/veil-<os>-<arch> が 1 つも見つかりません。" >&2
        echo "       先に 'tools/qemu/bsd-vm.sh <os> <arch> fetch' でバイナリを取得してください。" >&2
        exit 1
    fi
    exit 0
fi

# --from-qemu: bsd-vm.sh fetch が置いたバイナリと .os-version を使う
if (( FROM_QEMU )); then
    if [[ -z "${BINARY}" ]]; then
        BINARY="${BUILD_DIR}/veil-${OS}-${ARCH}"
    fi
    if [[ -z "${OS_VERSION}" && -f "${BINARY}.os-version" ]]; then
        OS_VERSION="$(cat "${BINARY}.os-version")"
    fi
fi

if [[ "${OS}" != "freebsd" && "${OS}" != "openbsd" && "${OS}" != "netbsd" ]]; then
    echo "ERROR: --os must be freebsd, openbsd, or netbsd" >&2; usage >&2; exit 1
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

# 配布パッケージのバイナリはシンボル表まで除去する（実測 36.8MB → 28.7MB、-22%）。
# release ビルドは元々 debug=0 で DWARF がほぼ無いため、効くのはシンボル表の除去である
# （`strip --strip-debug` は -1.7% にしかならない）。
#
# **ここで strip する理由**: BSD バイナリは `tools/qemu/bsd-vm.sh build` が VM 内で
# 作るが、そのバイナリは perf 計測（DTrace で関数名を見る）や E2E にも使う。
# ビルド時に strip すると診断が効かなくなるため、**配布パッケージを作る本スクリプトで
# だけ** strip する。strip(1) が無い環境では警告のみで続行する。
# STRIP_BINARY=0 を渡すと抑止できる（クラッシュ解析用に symbol を残したい場合）。
# `CARGO_PROFILE=dist` でビルドしたバイナリは既に strip 済み（profile.dist の
# strip = "symbols"）なので、ここでの strip は no-op になる。既定の `release` で
# ビルドしたバイナリ（perf 計測と同じもの）をパッケージ化する場合にのみ効く。
if [ "${STRIP_BINARY:-1}" = "1" ]; then
    if command -v strip >/dev/null 2>&1; then
        _before=$(wc -c < "${stage_parent}/${dir_name}/veil")
        strip --strip-all "${stage_parent}/${dir_name}/veil" 2>/dev/null \
            || strip "${stage_parent}/${dir_name}/veil" 2>/dev/null \
            || echo "WARNING: strip に失敗（サイズ削減なしで続行）" >&2
        _after=$(wc -c < "${stage_parent}/${dir_name}/veil")
        echo "==> strip: ${_before} -> ${_after} bytes"
    else
        echo "WARNING: strip(1) が見つからないためシンボル除去をスキップ" >&2
    fi
fi

# NetBSD は PaX MPROTECT がシステム全体で強制されており（security.pax.mprotect.*）、
# wasmtime の wasm 実行用 mmap/mprotect が EACCES で失敗する（B-60、OpenBSD の
# wxallowed/MAP_STACK 制約・B-52 の NetBSD 版に相当）。paxctl(8) は NetBSD 上にしか
# 無いツールのため、本スクリプトを NetBSD 上（paxctl 導入済み）で実行している場合は
# ここでパッケージ前に +m を適用してしまう。それ以外のホスト（通常はこちら。
# tools/qemu/bsd-vm.sh 側で VM 内ビルド直後に既に +m 済みのことが多い）では
# 適用できないため警告のみ表示し、INSTALL.txt 側にも導入手順として明記する。
if [[ "${OS}" == "netbsd" ]]; then
    if command -v paxctl >/dev/null 2>&1; then
        paxctl +m "${stage_parent}/${dir_name}/veil" \
            && echo "==> paxctl +m applied to packaged NetBSD binary" \
            || echo "WARNING: paxctl +m failed; see INSTALL.txt for the required manual step (B-60)" >&2
    else
        echo "NOTE: paxctl not available on this host; NetBSD package requires a manual" >&2
        echo "      'paxctl +m /usr/local/bin/veil' post-install step for wasm to work (B-60)." >&2
    fi
fi

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
    install -m 0555 "${BSD_ASSETS}/${OS}/veil.rc" "${stage_parent}/${dir_name}/rc.d/veil"
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
elif [[ "${OS}" == "openbsd" ]]; then
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
else
    cat >> "${stage_parent}/${dir_name}/INSTALL.txt" <<EOF
  mkdir -p /etc/veil
  cp config.toml.default /etc/veil/config.toml

  # rc.d サービス（NetBSD には rcctl が無いため rc.conf を直接編集する）
  install -m 0555 rc.d/veil /etc/rc.d/veil
  echo 'veil=YES' >> /etc/rc.conf
  /etc/rc.d/veil start

  # NetBSD には pledge/unveil 相当のランタイム API が無い（F-140）。veil が
  # 提供できるのは chroot(2)（config.toml の [security] chroot_dir）+
  # setuid/setgid による特権降格 + rlimit のみ。
  # TLS は rustls の ring プロバイダ + quiche 同梱 BoringSSL（full-netbsd）で動作する。

  # 重要（B-60）: NetBSD は PaX MPROTECT がシステム全体で有効になっており
  # （security.pax.mprotect.enabled / .global = 1）、これを無効化しないと
  # wasmtime の wasm 実行時 mmap/mprotect が EACCES で失敗し、Proxy-Wasm
  # フィルタが動かない（"on_request_headers error: Permission denied"）。
  # wasm 機能を使う場合は必ず以下を実行すること（本パッケージが自動で
  # 適用できなかった場合、あるいは配置先を変えた場合は都度実行する）:
  paxctl +m /usr/local/bin/veil
EOF
fi

tar -C "${stage_parent}" -czf "${OUTPUT_DIR}/${ARCHIVE_NAME}" "${dir_name}"
rm -rf "${stage_parent}"
echo "==> Created ${OUTPUT_DIR}/${ARCHIVE_NAME} (built on ${OS} ${OS_VERSION})"
