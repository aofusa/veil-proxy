#!/bin/sh
# =============================================================================
# lib.sh — BSD 3 OS（FreeBSD / NetBSD / OpenBSD）の差分を吸収する共通関数（F-176 N5）
# =============================================================================
# tools/perf/bsd/*.sh が `. "$(dirname "$0")/lib.sh"` で読み込む。
#
#   bsd_os              : freebsd | netbsd | openbsd
#   pin <cpus> <cmd...> : <cmd> を CPU 集合へ固定して実行する（exec）
#                         FreeBSD = cpuset -l、NetBSD = schedctl -A（自分を固定してから exec）、
#                         OpenBSD = 固定する API が無いので素の実行（結果に注記すること）
#   run_timeout <秒> <cmd...> : timeout(1) があればそれで、無ければそのまま実行する
#   bsd_tune            : listen backlog など計測前のカーネル設定（OS ごとの sysctl 名）
#   nginx_stream_module : nginx の stream 動的モジュールのパス（無ければ空）
#   pkg_hint            : 計測ツールの入れ方（エラーメッセージ用）
# =============================================================================

bsd_os=$(uname -s | tr 'A-Z' 'a-z')

pin() {
    _cpus="$1"; shift
    case "$bsd_os" in
        freebsd) exec cpuset -l "$_cpus" "$@" ;;
        # サブシェルの $$ は親シェルの PID なので、sh -c の中（= この後 exec する本人）で固定する。
        netbsd)  exec sh -c 'c="$1"; shift; schedctl -A "$c" -p $$ >/dev/null 2>&1; exec "$@"' sh "$_cpus" "$@" ;;
        *)       exec "$@" ;;
    esac
}

# サブシェルで pin する（呼び出し元のシェルを置き換えない）。
pinned() {
    ( pin "$@" )
}

run_timeout() {
    _secs="$1"; shift
    if command -v timeout >/dev/null 2>&1; then
        timeout "$_secs" "$@"
    elif command -v gtimeout >/dev/null 2>&1; then
        gtimeout "$_secs" "$@"
    else
        "$@"
    fi
}

bsd_tune() {
    case "$bsd_os" in
        freebsd)
            # kTLS の能力自体は有効化しておく（veil の既定計測は ktls_enabled = false）。
            sysctl kern.ipc.tls.enable=1 >/dev/null 2>&1 || true
            sysctl kern.ipc.somaxconn=4096 >/dev/null 2>&1 || true
            ;;
        netbsd)
            sysctl -w kern.somaxconn=4096 >/dev/null 2>&1 || true
            ;;
        openbsd)
            sysctl kern.somaxconn=4096 >/dev/null 2>&1 || true
            ;;
    esac
}

nginx_stream_module() {
    for so in /usr/local/libexec/nginx/ngx_stream_module.so \
              /usr/pkg/libexec/nginx/ngx_stream_module.so \
              /usr/local/lib/nginx/modules/ngx_stream_module.so; do
        [ -f "$so" ] && { echo "$so"; return 0; }
    done
    echo ""
}

pkg_hint() {
    case "$bsd_os" in
        freebsd) echo "pkg install nginx nghttp2 wrk-luajit curl" ;;
        netbsd)  echo "pkgin install nginx nghttp2 wrk curl" ;;
        openbsd) echo "pkg_add nginx nghttp2 wrk curl" ;;
    esac
}
