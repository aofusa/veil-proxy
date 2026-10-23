#!/bin/sh
# F-253: BSD の VM の中で、bsd-ports.py の port を各 OS の ports の仕組みでビルドし、入れて、サービスとして起動して確かめる。
#
#   sh tools/release/bsd-port-test.sh <freebsd|openbsd|netbsd> <tag> <out-dir>
#
# 1. 各 OS の ports の木を浅く取る（FreeBSD ports・OpenBSD ports・pkgsrc）
# 2. 依存はビルド済みのパッケージで入れる（ports の木で依存をソースからビルドしない）
# 3. 配布物（GitHub のタグの tarball と同じ形）を checkout した木から作って distfiles に置き、make makesum で distinfo を作る
# 4. make package → 入れる → サービスとして起動 → curl http://127.0.0.1:8080/
# <out-dir> に distinfo とパッケージ・ログを置く（提出のときに distinfo を使う）。
set -eu
OS="$1"; TAG="$2"; OUT="$3"
STEP="start"
# 失敗したら、どの段階で落ちたかと空き容量を出す（ログはログイン無しで読めないので、CI の注釈に出す）
trap 'rc=$?; [ "$rc" = 0 ] || { echo "PORT STEP FAILED: $STEP (exit $rc)"; df -h / /usr 2>/dev/null | tail -3; }' EXIT
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
VER="${TAG#v}"
mkdir -p "$OUT"; OUT="$(cd "$OUT" && pwd)"   # 下で ports の木へ cd するので絶対パスにする
log() { STEP="$*"; echo "==> $*"; }
check() { # <label> <cmd...>
    label="$1"; shift
    if "$@" >"$OUT/check.log" 2>&1; then echo "ok   $label"; else echo "FAIL $label"; tail -20 "$OUT/check.log"; FAILED=1; fi
}
FAILED=0

python3 "$ROOT/tools/release/bsd-ports.py" "$TAG" "$OUT/ports" 2>/dev/null || {
    # VM に python3 が無いときは入れる
    case "$OS" in
        freebsd) env ASSUME_ALWAYS_YES=yes pkg install -y python3 ;;
        openbsd) pkg_add -I python%3 ;;
        netbsd) PATH=/usr/pkg/bin:/usr/pkg/sbin:$PATH; pkgin -y install python313 && ln -sf python3.13 /usr/pkg/bin/python3 ;;
    esac
    python3 "$ROOT/tools/release/bsd-ports.py" "$TAG" "$OUT/ports"
}
# VEIL_PORT_LOCAL_SRC=1: GitHub のタグの tarball と同じ形（veil-proxy-<ver>/ の下に木）を checkout した木から作って使う
# （タグを打つ前の試験）。0（既定）なら、ports の仕組みが GitHub からタグの tarball を取る（distinfo は本物のチェックサム）
SRCTAR=""
if [ "${VEIL_PORT_LOCAL_SRC:-0}" = 1 ]; then
    SRCTAR="$OUT/src.tar.gz"
    ( cd "$ROOT" && git archive --format=tar.gz --prefix="veil-proxy-$VER/" -o "$SRCTAR" HEAD )
fi

case "$OS" in
freebsd)
    log "pkg install（依存）"
    env IGNORE_OSVERSION=yes ASSUME_ALWAYS_YES=yes pkg install -y git rust cmake-core gmake nasm protobuf bash curl pkgconf
    log "ports の木"
    # VM のディスクが小さい（8.5GB）ので、ports の仕組み（Mk など）だけを浅く取る。依存はパッケージで入れてある（NO_DEPENDS）
    if [ ! -d /usr/ports/Mk ]; then
        git clone -q --depth 1 --filter=blob:none --sparse https://git.FreeBSD.org/ports.git /usr/ports
        git -C /usr/ports sparse-checkout set Mk Templates Keywords Tools www/nginx
    fi
    LLVM_DEFAULT="$(make -C /usr/ports/www/nginx -V LLVM_DEFAULT 2>/dev/null || echo 19)"
    env ASSUME_ALWAYS_YES=yes pkg install -y "llvm${LLVM_DEFAULT}"
    mkdir -p "$(dirname /usr/ports/www/veil)" && rm -rf /usr/ports/www/veil && cp -R "$OUT/ports/freebsd/www/veil" /usr/ports/www/veil
    cd /usr/ports/www/veil
    DIST="$(make -V DISTDIR)"; mkdir -p "$DIST"
    [ -n "$SRCTAR" ] && cp "$SRCTAR" "$DIST/$(make -V DISTFILES | tr ' ' '\n' | grep -E '_GH0\.tar\.gz' | sed 's/:.*//')"
    log "make makesum（クレートを取る）"
    make makesum BATCH=yes >"$OUT/makesum.log" 2>&1 || { tail -30 "$OUT/makesum.log"; exit 1; }
    cp distinfo "$OUT/distinfo.freebsd"
    log "make package"
    make package BATCH=yes NO_DEPENDS=yes >"$OUT/build.log" 2>&1 || { grep -E 'error|Error' "$OUT/build.log" | tail -30; tail -30 "$OUT/build.log"; exit 1; }
    check "portlint-like: make check-plist" make check-plist BATCH=yes
    PKG="$(ls work/pkg/veil-*.pkg)"; cp "$PKG" "$OUT/"
    env ASSUME_ALWAYS_YES=yes pkg add "$PKG"
    check "config at /usr/local/etc/veil/config.toml" test -f /usr/local/etc/veil/config.toml
    sysrc veil_enable=YES >/dev/null
    check "service veil start" service veil start
    sleep 3
    check "GET / on 127.0.0.1:8080" sh -c 'curl -fsS http://127.0.0.1:8080/quickstart.html | grep -q "veil quickstart"'
    check "default path built in" sh -c '/usr/local/bin/veil --help | grep -q /usr/local/etc/veil/config.toml'
    check "runs as www" sh -c 'ps -axo user,comm | grep -E "^www +veil" >/dev/null'
    check "service veil reload" service veil reload
    check "service veil stop" service veil stop
    check "pkg delete" env ASSUME_ALWAYS_YES=yes pkg delete -y veil
    ;;
openbsd)
    P="https://cdn.openbsd.org/pub/OpenBSD/$(uname -r)/packages/$(uname -p)/"
    log "pkg_add（依存）"
    env PKG_PATH="$P" pkg_add -I git rust cmake gmake protobuf bash
    LLVM="$(env PKG_PATH="$P" pkg_info -Q llvm 2>/dev/null | grep -E '^llvm-[0-9]' | sort -V | tail -1)"; env PKG_PATH="$P" pkg_add -I "$LLVM"
    log "ports の木"
    if [ ! -d /usr/ports/infrastructure ]; then
        git clone -q --depth 1 --filter=blob:none --sparse https://github.com/openbsd/ports.git /usr/ports
        git -C /usr/ports sparse-checkout set infrastructure devel/cargo lang/rust lang/clang lang/python devel/llvm
    fi
    mkdir -p "$(dirname /usr/ports/www/veil)" && rm -rf /usr/ports/www/veil && cp -R "$OUT/ports/openbsd/www/veil" /usr/ports/www/veil
    cd /usr/ports/www/veil
    DIST="$(make show=FULLDISTDIR)"; mkdir -p "$DIST"
    [ -n "$SRCTAR" ] && cp "$SRCTAR" "$DIST/$(make show=DISTFILES | tr ' ' '\n' | grep -i veil-proxy | sed 's/{.*}//; s/:.*//')"
    log "make makesum"
    make makesum >"$OUT/makesum.log" 2>&1 || { tail -30 "$OUT/makesum.log"; exit 1; }
    cp distinfo "$OUT/distinfo.openbsd"
    log "make package"
    ulimit -d unlimited 2>/dev/null || true
    make package NO_DEPENDS=Yes >"$OUT/build.log" 2>&1 || { grep -E 'error' "$OUT/build.log" | tail -30; tail -30 "$OUT/build.log"; exit 1; }
    PKG="$(ls /usr/ports/packages/*/all/veil-*.tgz | head -1)"; cp "$PKG" "$OUT/"
    env TRUSTED_PKG_PATH=/usr/ports/packages/$(uname -p)/all pkg_add -D unsigned "$PKG"
    check "config at /etc/veil/config.toml" test -f /etc/veil/config.toml
    check "rcctl enable veil" rcctl enable veil
    check "rcctl start veil" rcctl start veil
    sleep 3
    check "GET / on 127.0.0.1:8080" sh -c 'curl -fsS http://127.0.0.1:8080/quickstart.html | grep -q "veil quickstart"'
    check "runs as _veil" sh -c 'ps -axo user,comm | grep -E "^_veil +veil" >/dev/null'
    check "rcctl reload veil" rcctl reload veil
    check "rcctl stop veil" rcctl stop veil
    check "pkg_delete" pkg_delete veil
    ;;
netbsd)
    export PATH="/usr/pkg/bin:/usr/pkg/sbin:/usr/sbin:/sbin:$PATH"
    export PKG_PATH="https://cdn.NetBSD.org/pub/pkgsrc/packages/NetBSD/$(uname -m | sed 's/amd64/x86_64/')/$(uname -r | cut -d. -f1).0/All/"
    log "pkgin（依存）"
    [ -x /usr/pkg/bin/pkgin ] || /usr/sbin/pkg_add pkgin
    pkgin -y update
    pkgin -y install git-base rust cmake gmake protobuf bash clang nasm pkgconf mozilla-rootcerts-openssl
    log "pkgsrc の木"
    if [ ! -d /usr/pkgsrc/mk ]; then
        git clone -q --depth 1 --filter=blob:none --sparse https://github.com/NetBSD/pkgsrc.git /usr/pkgsrc
        git -C /usr/pkgsrc sparse-checkout set mk lang/rust lang/python devel/cmake devel/protobuf shells/bash lang/clang \
            devel/nasm devel/gmake pkgtools
    fi
    mkdir -p "$(dirname /usr/pkgsrc/www/veil)" && rm -rf /usr/pkgsrc/www/veil && cp -R "$OUT/ports/netbsd/www/veil" /usr/pkgsrc/www/veil
    cd /usr/pkgsrc/www/veil
    DIST="$(make show-var VARNAME=DISTDIR)"; mkdir -p "$DIST"
    [ -n "$SRCTAR" ] && cp "$SRCTAR" "$DIST/$(make show-var VARNAME=DISTFILES | tr ' ' '\n' | grep veil-proxy)"
    log "make makesum"
    make makesum >"$OUT/makesum.log" 2>&1 || { tail -30 "$OUT/makesum.log"; exit 1; }
    cp distinfo "$OUT/distinfo.netbsd"
    log "make package"
    make package >"$OUT/build.log" 2>&1 || { grep -E 'error' "$OUT/build.log" | tail -30; tail -30 "$OUT/build.log"; exit 1; }
    PKG="$(ls /usr/pkgsrc/packages/All/veil-*.tgz | head -1)"; cp "$PKG" "$OUT/"
    pkg_add "$PKG"
    cp /usr/pkg/share/examples/rc.d/veil /etc/rc.d/veil
    check "config at /usr/pkg/etc/veil/config.toml" test -f /usr/pkg/etc/veil/config.toml
    echo veil=YES >> /etc/rc.conf
    check "service veil start" /etc/rc.d/veil start
    sleep 3
    check "GET / on 127.0.0.1:8080" sh -c 'curl -fsS http://127.0.0.1:8080/quickstart.html | grep -q "veil quickstart"'
    check "runs as veil" sh -c 'ps -axo user,comm | grep -E "^veil +veil" >/dev/null'
    check "service veil stop" /etc/rc.d/veil stop
    check "pkg_delete" pkg_delete veil
    ;;
esac
[ "$FAILED" = 0 ] && echo "PORT TEST ($OS): ALL OK" || { echo "PORT TEST ($OS): FAILED"; exit 1; }
