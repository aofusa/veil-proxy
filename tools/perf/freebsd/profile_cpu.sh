#!/bin/sh
# =============================================================================
# profile_cpu.sh — veil の CPU を関数別にサンプリングする（FreeBSD / DTrace profile）
# =============================================================================
# 使い方（FreeBSD ゲスト内・root。run_perf_freebsd.sh を一度実行して ${WORK} がある状態）:
#   sh tools/perf/freebsd/profile_cpu.sh <h1_file_plain|h1_file_tls|h1_proxy_tls|h3_file|l4_tcp> [path]
# 出力: ユーザー空間の関数別サンプル数（上位）とカーネル関数別サンプル数（上位）
# =============================================================================
set -eu
WORK="${WORK:-/tmp/veilperf}"
REPO="${REPO:-$(cd "$(dirname "$0")/../../.." && pwd)}"
VEIL_BIN="${VEIL_BIN:-${REPO}/target/release/veil}"
SCEN="${1:-h1_file_plain}"
REQ_PATH="${2:-/small.html}"
DUR="${DUR:-10}"
case "$SCEN" in
  h1_file_plain) cfg=veil_file.toml;    url="http://127.0.0.1:4443${REQ_PATH}" ;;
  h1_file_tls)   cfg=veil_file.toml;    url="https://127.0.0.1:4443${REQ_PATH}" ;;
  h1_proxy_tls)  cfg=veil_proxy.toml;   url="https://127.0.0.1:4443${REQ_PATH}" ;;
  h3_file)       cfg=veil_file_h3.toml; url="https://127.0.0.1:4443${REQ_PATH}" ;;
  l4_tcp)        cfg=veil_l4.toml;      url="http://127.0.0.1:4090${REQ_PATH}" ;;
  *) echo "unknown scenario" >&2; exit 1 ;;
esac
kldload dtraceall 2>/dev/null || true
pkill -x veil 2>/dev/null || true; sleep 1
pgrep -x nginx >/dev/null || nginx -c "${WORK}/conf/nginx.conf"
cpuset -l 0,1 "${VEIL_BIN}" -c "${WORK}/conf/${cfg}" > "${WORK}/logs/veil_prof.log" 2>&1 &
sleep 2
if [ "$SCEN" = h3_file ]; then
  (cpuset -l 2,3 "${REPO}/tools/perf/h3load/target/release/h3load" -t2 -c64 -m 32 -d "$DUR" "$url" >/dev/null 2>&1 &)
else
  (cpuset -l 2,3 wrk -t2 -c64 -d"${DUR}"s "$url" >/dev/null 2>&1 &)
fi
sleep 2
rm -f "${WORK}/prof.txt"  # dtrace -o は追記するので毎回消す
dtrace -q -x ustackframes=1 -n "profile-1999 /execname == \"veil\" && arg1/ { @[ufunc(arg1)] = count(); } profile-1999 /execname == \"veil\" && arg0/ { @k[func(arg0)] = count(); } tick-$((DUR - 4))s { exit(0); }" \
  -o "${WORK}/prof.txt"
pkill -x veil || true
echo "== top functions (user + kernel samples, $SCEN)"
sort -k2 -n -r "${WORK}/prof.txt" | awk 'NF==2' | head -45
