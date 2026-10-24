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
# （CI では VM に入る前に runner が <out-dir>/src.tar.gz を git archive で作っておく。VM にはまだ git が無い）
SRCTAR=""
if [ "${VEIL_PORT_LOCAL_SRC:-0}" = 1 ]; then
    SRCTAR="$OUT/src.tar.gz"
    [ -s "$SRCTAR" ] || ( cd "$ROOT" && git archive --format=tar.gz --prefix="veil-proxy-$VER/" -o "$SRCTAR" HEAD )
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
    make package BATCH=yes NO_DEPENDS=yes >"$OUT/build.log" 2>&1 || { grep -E '^error|error(\[|:)|panicked|Could not|not found' "$OUT/build.log" | grep -v 'Running `' | tail -30; tail -30 "$OUT/build.log"; exit 1; }
    check "portlint-like: make check-plist" make check-plist BATCH=yes
    PKG="$(ls work/pkg/veil-*.pkg)"; cp "$PKG" "$OUT/"
    log "入れる・サービスの試験"
    env ASSUME_ALWAYS_YES=yes pkg add "$PKG"
    check "config at /usr/local/etc/veil/config.toml" test -f /usr/local/etc/veil/config.toml
    sysrc veil_enable=YES >/dev/null
    check "service veil start" service veil start
    sleep 3
    check "GET / on 127.0.0.1:8080" sh -c 'curl -fsS http://127.0.0.1:8080/quickstart.html | grep -q "veil quickstart"'
    check "default path built in" sh -c '/usr/local/bin/veil --help | grep -q /usr/local/etc/veil/config.toml'
    check "runs as www" sh -c 'ps -axo user,args | grep -E "^www +[^ ]*veil( |$)" >/dev/null'
    check "service veil reload" service veil reload
    sleep 2
    check "GET / after reload" sh -c 'curl -fsS http://127.0.0.1:8080/quickstart.html | grep -q "veil quickstart"'
    check "reload reached veil" grep -q "SIGHUP received" /var/log/veil.log
    check "service veil stop" service veil stop
    sleep 2
    check "no veil left after stop" sh -c '! pgrep -x veil'
    check "pkg delete" env ASSUME_ALWAYS_YES=yes pkg delete -y veil
    ;;
openbsd)
    P="https://cdn.openbsd.org/pub/OpenBSD/$(uname -r)/packages/$(uname -p)/"
    log "pkg_add（依存）"
    # cargo-generate-vendor: devel/cargo の module がクレートの metadata を作るのに使う
    env PKG_PATH="$P" pkg_add -I git rust cmake gmake protobuf bash cargo-generate-vendor
    log "ports の木"
    # ports の木はパッケージと同じ版（リリースの ports.tar.gz）を使う。GitHub の mirror は -current だけで、
    # -current の devel/cargo はリリースの cargo-generate-vendor が知らない引数を渡す。package を作るときに
    # 依存の依存まで ports の木を辿る（archivers/zstd → archivers/lz4 …）ので、一部ではなく全部を展開する
    if [ ! -d /usr/ports/infrastructure ]; then
        ftp -o - "https://cdn.openbsd.org/pub/OpenBSD/$(uname -r)/ports.tar.gz" | tar -xzf - -C /usr
    fi
    mkdir -p "$(dirname /usr/ports/www/veil)" && rm -rf /usr/ports/www/veil && cp -R "$OUT/ports/openbsd/www/veil" /usr/ports/www/veil
    # @newuser は infrastructure/db/user.list に登録されていないと pkg_create が止まる（提出のときに空きの番号を取る。
    # 試験では空いている番号で登録する）
    UL=/usr/ports/infrastructure/db/user.list
    if ! grep -q '[[:space:]]_veil[[:space:]]' "$UL"; then
        # 雛形の番号（bsd-ports.py の OPENBSD_UID）が使われていたら、空いている番号に替える
        uid="$(sed -n 's|^@newuser _veil:\([0-9]*\):.*|\1|p' /usr/ports/www/veil/pkg/PLIST)"
        while awk -v u="$uid" '$1 == u { f = 1 } END { exit !f }' "$UL"; do uid=$((uid + 1)); done
        sed -i "s|^@newgroup _veil:[0-9]*|@newgroup _veil:$uid|; s|^@newuser _veil:[0-9]*:|@newuser _veil:$uid:|" /usr/ports/www/veil/pkg/PLIST
        echo "user.list: _veil gets id $uid"
        # 「番号 利用者 group port」の 1 行で利用者と同じ番号の group を表す（group の行を別に足すと Duplicate id）
        printf '%s _veil\t\t_veil\t\twww/veil\n' "$uid" >> "$UL"
    fi
    grep -n '_veil\|^# *[A-Z]' "$UL" | tail -8
    cd /usr/ports/www/veil
    # lang/clang の module が使う版の llvm（LIBCLANG_PATH は ${LOCALBASE}/llvm${MODCLANG_VERSION}/lib）
    MODCLANG_VERSION="$(make show=MODCLANG_VERSION)"; echo "MODCLANG_VERSION=$MODCLANG_VERSION"
    env PKG_PATH="$P" pkg_add -I "llvm%$MODCLANG_VERSION"
    DIST="$(make show=FULLDISTDIR)"; mkdir -p "$DIST"
    # DIST_TUPLE（github）の配布物は DISTFILES に出ない。名前は <account>-<project>-<tag>.tar.gz
    [ -n "$SRCTAR" ] && cp "$SRCTAR" "$DIST/aofusa-veil-proxy-$TAG.tar.gz"
    log "make makesum"
    make makesum >"$OUT/makesum.log" 2>&1 || { tail -30 "$OUT/makesum.log"; exit 1; }
    cp distinfo "$OUT/distinfo.openbsd"
    log "make package"
    ulimit -d unlimited 2>/dev/null || true
    make package NO_DEPENDS=Yes >"$OUT/build.log" 2>&1 || { grep -E '^error|error(\[|:)|panicked|Could not|not found' "$OUT/build.log" | grep -v 'Running `' | tail -30; tail -30 "$OUT/build.log"; exit 1; }
    PKG="$(ls /usr/ports/packages/*/all/veil-*.tgz | head -1)"; cp "$PKG" "$OUT/"
    log "入れる・サービスの試験"
    env TRUSTED_PKG_PATH=/usr/ports/packages/$(uname -p)/all pkg_add -D unsigned "$PKG"
    check "config at /etc/veil/config.toml" test -f /etc/veil/config.toml
    check "rcctl enable veil" rcctl enable veil
    check "rcctl start veil" rcctl start veil
    sleep 3
    check "GET / on 127.0.0.1:8080" sh -c 'curl -fsS http://127.0.0.1:8080/quickstart.html | grep -q "veil quickstart"'
    check "runs as _veil" sh -c 'ps -axo user,args | grep -E "^_veil +[^ ]*veil( |$)" >/dev/null'
    check "rcctl reload veil" rcctl reload veil
    sleep 2
    check "GET / after reload" sh -c 'curl -fsS http://127.0.0.1:8080/quickstart.html | grep -q "veil quickstart"'
    check "rcctl stop veil" rcctl stop veil
    sleep 2
    check "no veil left after stop" sh -c '! pgrep -x veil'
    check "pkg_delete" pkg_delete veil
    ;;
netbsd)
    export PATH="/usr/pkg/bin:/usr/pkg/sbin:/usr/sbin:/sbin:$PATH"
    export PKG_PATH="https://cdn.NetBSD.org/pub/pkgsrc/packages/NetBSD/$(uname -m | sed 's/amd64/x86_64/')/$(uname -r | cut -d. -f1).0/All/"
    log "pkgin（依存）"
    [ -x /usr/pkg/bin/pkgin ] || /usr/sbin/pkg_add pkgin
    pkgin -y update
    # digest: make makesum・checksum が使う（無いと distinfo にクレートの SHA512 が入らず、checksum で止まる）
    pkgin -y install git-base rust cmake gmake protobuf bash clang nasm pkgconf mozilla-rootcerts-openssl digest
    # pkgsrc は PKG_PATH があると止まる（pkg_add の取得先。pkgin は repositories.conf を使う）。足りない依存は
    # ソースからビルドせず（clang などは何時間もかかる）、同じ取得先のバイナリのパッケージで入れる
    grep -q '^DEPENDS_TARGET' /etc/mk.conf 2>/dev/null || printf 'DEPENDS_TARGET=\tbin-install\nBINPKG_SITES=\t%s\n' "${PKG_PATH%/All/}" >> /etc/mk.conf
    unset PKG_PATH
    log "pkgsrc の木"
    if [ ! -d /usr/pkgsrc/mk ]; then
        # バイナリのパッケージ（pkgin）と同じ四半期の枝を使う（HEAD の lang/rust・cargo.mk はパッケージとずれる）
        BRANCH="$(git ls-remote --heads https://github.com/NetBSD/pkgsrc.git 'pkgsrc-20*' | sed 's|.*refs/heads/||' | sort | tail -1)"
        echo "pkgsrc branch: $BRANCH"
        git clone -q --depth 1 --branch "$BRANCH" --filter=blob:none --sparse https://github.com/NetBSD/pkgsrc.git /usr/pkgsrc
        git -C /usr/pkgsrc sparse-checkout set mk lang/rust lang/python devel/cmake devel/protobuf shells/bash lang/clang \
            devel/nasm devel/gmake pkgtools
    fi
    mkdir -p "$(dirname /usr/pkgsrc/www/veil)" && rm -rf /usr/pkgsrc/www/veil && cp -R "$OUT/ports/netbsd/www/veil" /usr/pkgsrc/www/veil
    cd /usr/pkgsrc/www/veil
    # port の依存（DEPENDS・TOOL_DEPENDS の ../../<cat>/<pkg>）も木に要る（無いと依存を入れる所で
    # 「reduce-depends.awk: ... does not exist」で止まる）
    # shellcheck disable=SC2046
    git -C /usr/pkgsrc sparse-checkout add $(sed -n 's|.*\.\./\.\./\([a-z0-9]*/[A-Za-z0-9_.+-]*\).*|\1|p' Makefile | grep -v "^mk/" | sort -u)
    # buildlink3.mk・compiler の mk は他の package の makefile を間接に読む（lang/libunwind・lang/gccNN など）。
    # 足りないと「Could not find ../../<cat>/<pkg>/...」で止まるので、それを sparse の木に足して読み直す
    i=0
    while miss="$(make show-var VARNAME=PKGNAME 2>&1 | sed -n 's|.*Could not find \.\./\.\./\([^/]*/[^/]*\)/.*|\1|p' | sort -u)"; [ -n "$miss" ]; do
        i=$((i + 1)); [ "$i" -le 20 ] || { echo "pkgsrc: still missing: $miss"; exit 1; }
        echo "pkgsrc: adding $miss"
        # shellcheck disable=SC2086
        git -C /usr/pkgsrc sparse-checkout add $miss
    done
    # 依存を入れる所（reduce-depends.awk）は依存の dir も読む（buildlink3.mk が足す devel/pkgconf など）。
    # 「... does not exist」の dir を疎な木に足して入れ直す。distfiles を置く前に済ませる（sparse-checkout は
    # 木の外の無視されたファイルだけの dir＝distfiles を消す）
    i=0
    while ! make depends >"$OUT/depends.log" 2>&1; do
        miss="$(sed -n 's|.*reduce-depends.awk\] \.\./\.\./\([^ ]*\) does not exist.*|\1|p' "$OUT/depends.log" | sort -u)"
        i=$((i + 1)); [ -n "$miss" ] && [ "$i" -le 20 ] || { tail -30 "$OUT/depends.log"; exit 1; }
        echo "pkgsrc: adding $miss"
        # shellcheck disable=SC2086
        git -C /usr/pkgsrc sparse-checkout add $miss
    done
    DIST="$(make show-var VARNAME=DISTDIR)"; mkdir -p "$DIST"
    [ -n "$SRCTAR" ] && cp "$SRCTAR" "$DIST/$(make show-var VARNAME=DISTFILES | tr ' ' '\n' | grep veil-proxy)"
    log "make makesum"
    make makesum >"$OUT/makesum.log" 2>&1 || { tail -30 "$OUT/makesum.log"; exit 1; }
    # クレートが distinfo に入ったか（入っていなければ make package の checksum で止まる）
    echo "distfiles: $(make show-var VARNAME=DISTFILES | wc -w), in distinfo: $(grep -c '^SHA512' distinfo)"
    cp distinfo "$OUT/distinfo.netbsd"
    log "make package"
    make package >"$OUT/build.log" 2>&1 || { grep -E '^error|error(\[|:)|panicked|Could not|not found' "$OUT/build.log" | grep -v 'Running `' | tail -30; tail -30 "$OUT/build.log"; exit 1; }
    PKG="$(ls /usr/pkgsrc/packages/All/veil-*.tgz | head -1)"; cp "$PKG" "$OUT/"
    log "入れる・サービスの試験"
    pkg_add "$PKG"
    cp /usr/pkg/share/examples/rc.d/veil /etc/rc.d/veil
    check "config at /usr/pkg/etc/veil/config.toml" test -f /usr/pkg/etc/veil/config.toml
    echo veil=YES >> /etc/rc.conf
    check "service veil start" /etc/rc.d/veil start
    sleep 3
    check "GET / on 127.0.0.1:8080" sh -c 'curl -fsS http://127.0.0.1:8080/quickstart.html | grep -q "veil quickstart"'
    check "runs as veil" sh -c 'ps -axo user,args | grep -E "^veil +[^ ]*veil( |$)" >/dev/null'
    # 降格の結果（NetBSD の ps の comm は実行ファイルのパスで、桁で切れる。上の検査は args の先頭で見る）
    ps -axo user,uid,args | grep '[v]eil' || true
    check "service veil stop" /etc/rc.d/veil stop
    sleep 2
    check "no veil left after stop" sh -c '! pgrep -x veil'
    check "pkg_delete" pkg_delete veil
    ;;
esac
[ "$FAILED" = 0 ] && echo "PORT TEST ($OS): ALL OK" || { echo "PORT TEST ($OS): FAILED"; exit 1; }
