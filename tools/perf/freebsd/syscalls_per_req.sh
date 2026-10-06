#!/bin/sh
# =============================================================================
# syscalls_per_req.sh — 1 リクエストあたりの syscall 数を veil / nginx で比較する（FreeBSD）
# =============================================================================
#
# run_perf_freebsd.sh を一度実行して ${WORK}（既定 /tmp/veilperf）に設定・配信ファイルが
# できている状態で、指定シナリオのサーバを起動し、wrk で負荷をかけながら DTrace で
# サーバプロセスの syscall を数え、wrk の総リクエスト数で割って出力する。
#
# 使い方（FreeBSD ゲスト内・root、kldload dtraceall 済み）:
#   sh tools/perf/freebsd/syscalls_per_req.sh <h1_file_plain|h1_file_tls|h1_proxy_tls|h3_file|l4_tcp> [path]
#   （h3_file は tools/perf/h3load をビルド済みであること）
# =============================================================================
set -eu
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
  *) echo "unknown scenario $SCEN" >&2; exit 1 ;;
esac

kldstat -q -m dtrace_test 2>/dev/null || kldload dtraceall 2>/dev/null || true
pkill -x veil 2>/dev/null || true; sleep 1
pgrep -x nginx >/dev/null || nginx -c "${WORK}/conf/nginx.conf"
cpuset -l 0,1 "${VEIL_BIN}" -c "${WORK}/conf/${cfg}" > "${WORK}/logs/veil_sc.log" 2>&1 &
sleep 2

measure() {  # name url execname
    rm -f "${WORK}/sc_$1.txt"  # dtrace -o は追記するので毎回消す
    dtrace -q -n "syscall:::entry /execname == \"$3\"/ { @[probefunc] = count(); } tick-${DUR}s { exit(0); }" \
        -o "${WORK}/sc_$1.txt" &
    dpid=$!
    sleep 1
    if [ "$SCEN" = h3_file ]; then
        reqs=$(cpuset -l 2,3 "${REPO}/tools/perf/h3load/target/release/h3load" -t2 -c64 -m 32 \
            -d "$((DUR - 2))" "$2" 2>/dev/null | awk '/^requests:/ {print $8}')
    else
        reqs=$(cpuset -l 2,3 wrk -t2 -c64 -d"$((DUR - 2))"s "$2" 2>/dev/null | awk '/requests in/ {print $1}')
    fi
    wait $dpid
    total=$(awk 'NF==2 {s+=$2} END {print s+0}' "${WORK}/sc_$1.txt")
    echo "== $1: requests=${reqs} syscalls=${total} per_req=$(echo "scale=2; ${total}/${reqs}" | bc)"
    sort -k2 -n -r "${WORK}/sc_$1.txt" | awk -v r="$reqs" 'NF==2 && $2/r > 0.01 {printf "   %-16s %8.3f/req\n", $1, $2/r}' | head -15
}
measure "veil_${SCEN}" "$v" veil
measure "nginx_${SCEN}" "$n" nginx
pkill -x veil || true
