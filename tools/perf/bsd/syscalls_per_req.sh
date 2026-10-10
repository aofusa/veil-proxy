#!/bin/sh
# =============================================================================
# syscalls_per_req.sh — 1 リクエストあたりの syscall 数を veil / nginx で比較する（BSD）
# =============================================================================
#
# run_perf_bsd.sh を一度実行して ${WORK}（既定 /tmp/veilperf）に設定・配信ファイルが
# できている状態で、指定シナリオのサーバを起動し、wrk で負荷をかけながらサーバプロセスの
# syscall を数え、総リクエスト数で割って出力する。数え方は FreeBSD = DTrace、
# NetBSD / OpenBSD = ktrace(1) + kdump(1)（F-176）。
#
# 使い方（BSD ゲスト内・root）:
#   sh tools/perf/bsd/syscalls_per_req.sh <h1_file_plain|h1_file_tls|h1_proxy_tls|h3_file|l4_tcp|h2c_file_plain> [path]
#   （h3_file は tools/perf/h3load をビルド済みであること）
# =============================================================================
set -eu
. "$(cd "$(dirname "$0")" && pwd)/lib.sh"
WORK="${WORK:-/tmp/veilperf}"
REPO="${REPO:-$(cd "$(dirname "$0")/../../.." && pwd)}"
VEIL_BIN="${VEIL_BIN:-${REPO}/target/release/veil}"
SCEN="${1:-h1_file_plain}"
REQ_PATH="${2:-/small.html}"
DUR="${DUR:-8}"

case "$SCEN" in
  h1_file_plain) cfg=veil_file.toml;  v="http://127.0.0.1:4443${REQ_PATH}";  n="http://127.0.0.1:5080${REQ_PATH}" ;;
  h1_file_tls)   cfg=veil_file.toml;  v="https://127.0.0.1:4443${REQ_PATH}"; n="https://127.0.0.1:5443${REQ_PATH}" ;;
  h1_proxy_tls)  cfg=veil_proxy.toml; v="https://127.0.0.1:4443${REQ_PATH}"; n="https://127.0.0.1:5443/proxy${REQ_PATH}" ;;
  h3_file)       cfg=veil_file_h3.toml; v="https://127.0.0.1:4443${REQ_PATH}"; n="https://127.0.0.1:5443${REQ_PATH}" ;;
  l4_tcp)        cfg=veil_l4.toml;      v="http://127.0.0.1:4090${REQ_PATH}";  n="http://127.0.0.1:5090${REQ_PATH}" ;;
  h2c_file_plain) cfg=veil_file.toml;   v="http://127.0.0.1:4080${REQ_PATH}";  n="http://127.0.0.1:5080${REQ_PATH}" ;;
  *) echo "unknown scenario $SCEN" >&2; exit 1 ;;
esac

[ "$bsd_os" = freebsd ] && { kldstat -q -m dtrace_test 2>/dev/null || kldload dtraceall 2>/dev/null || true; }
pkill -x veil 2>/dev/null || true; sleep 1
pgrep -x nginx >/dev/null || nginx -c "${WORK}/conf/nginx.conf"
pinned 0,1 "${VEIL_BIN}" -c "${WORK}/conf/${cfg}" > "${WORK}/logs/veil_sc.log" 2>&1 &
sleep 2

# 負荷をかけて総リクエスト数を返す。
load() {  # url
    if [ "$SCEN" = h2c_file_plain ]; then
        pinned 2,3 h2load -t2 -c64 -m 32 -D "$((DUR - 2))" "$1" 2>/dev/null \
            | awk '/^requests:/ {print $8}'
    elif [ "$SCEN" = h3_file ]; then
        pinned 2,3 "${REPO}/tools/perf/h3load/target/release/h3load" -t2 -c64 -m 32 \
            -d "$((DUR - 2))" "$1" 2>/dev/null | awk '/^requests:/ {print $8}'
    else
        pinned 2,3 wrk -t2 -c64 -d"$((DUR - 2))"s "$1" 2>/dev/null | awk '/requests in/ {print $1}'
    fi
}

# ktrace の出力から syscall 名ごとの回数を "<name> <count>" で出す（NetBSD / OpenBSD）。
kdump_counts() {  # ktrace.out
    kdump -f "$1" 2>/dev/null | awk '{
        for (i = 1; i <= NF; i++) if ($i == "CALL") { n = $(i + 1); sub(/\(.*/, "", n); c[n]++; break }
    } END { for (k in c) print k, c[k] }'
}

measure() {  # name url execname
    out="${WORK}/sc_$1.txt"
    rm -f "$out"  # dtrace -o は追記するので毎回消す
    if [ "$bsd_os" = freebsd ]; then
        dtrace -q -n "syscall:::entry /execname == \"$3\"/ { @[probefunc] = count(); } tick-${DUR}s { exit(0); }" \
            -o "$out" &
        dpid=$!
        sleep 1
        reqs=$(load "$2")
        wait $dpid
    else
        for p in $(pgrep -x "$3"); do ktrace -t c -f "${WORK}/kt_$1.out" -p "$p" 2>/dev/null || true; done
        sleep 1
        reqs=$(load "$2")
        for p in $(pgrep -x "$3"); do ktrace -c -p "$p" 2>/dev/null || true; done
        kdump_counts "${WORK}/kt_$1.out" > "$out"
        rm -f "${WORK}/kt_$1.out"
    fi
    total=$(awk 'NF==2 {s+=$2} END {print s+0}' "$out")
    echo "== $1: requests=${reqs} syscalls=${total} per_req=$(echo "scale=2; ${total}/${reqs}" | bc)"
    sort -k2 -n -r "$out" | awk -v r="$reqs" 'NF==2 && $2/r > 0.01 {printf "   %-16s %8.3f/req\n", $1, $2/r}' | head -15
}
measure "veil_${SCEN}" "$v" veil
measure "nginx_${SCEN}" "$n" nginx
pkill -x veil || true
