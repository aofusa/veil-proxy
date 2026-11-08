#!/bin/sh
# =============================================================================
# version_ab_bsd.sh — 2 つの veil のビルドを交互に測る（BSD、リリース間の退行確認。v0.12.0）
# =============================================================================
#
# cpu_per_req.sh の veil 側だけを 2 つのバイナリで回す。シナリオごとに A・B を交互に（ラウンドごとに順を入れ替えて）
# 測り、rps と「サーバの CPU 時間 / 要求」（ns）を出す。VM はホストの負荷で rps が大きく揺れるので、効率の比較は
# CPU/要求を主にする。
#
# 前提:
#   - run_perf_bsd.sh を一度実行して ${WORK}（既定 /tmp/veilperf）に設定・配信ファイルがある（上流の nginx-up も使う）
#   - h3_file を測るなら h3load をビルドしておく（cargo build --release --manifest-path tools/perf/h3load/Cargo.toml）
#   - 比べる 2 つのバイナリは、それぞれ別のディレクトリに **`veil` という名前で** 置く。`pkill -x veil` で止まらない名前に
#     すると、SO_REUSEPORT で前のプロセスが同じポートで受け続け、計測が混ざる
#
# 計測対象は 0,1 コア、負荷生成と上流は 2,3 コアに固定する。
# 出力: scenario size build round rps cpu_ns_per_req（build は A_NAME・B_NAME）
#
# 使い方（BSD ゲスト内・root）:
#   A_DIR=/root/ab/old B_DIR=/root/ab/new sh tools/perf/bsd/version_ab_bsd.sh > ab.tsv
# 環境変数: A_DIR・B_DIR（必須）、A_NAME（old）・B_NAME（new）、ROUNDS（4）、DUR（8 秒）、
#           PATHS（"/small.html /" = 3B と 54KB）、SCEN（8 シナリオ全部）、WORK（/tmp/veilperf）
# =============================================================================
set -u
. "$(cd "$(dirname "$0")" && pwd)/lib.sh"
REPO="${REPO:-$(cd "$(dirname "$0")/../../.." && pwd)}"
cd "$REPO"
: "${A_DIR:?set A_DIR (a directory with the first veil binary)}"
: "${B_DIR:?set B_DIR (a directory with the second veil binary)}"
A_NAME=${A_NAME:-old}; B_NAME=${B_NAME:-new}
WORK="${WORK:-/tmp/veilperf}"; DUR=${DUR:-8}; ROUNDS=${ROUNDS:-4}
for d in "$A_DIR" "$B_DIR"; do
  [ -x "$d/veil" ] || { echo "no executable veil in $d" >&2; exit 1; }
done
cpu() { ps -o time= -p "$1" | awk -F'[:.]' '{print ($1*60+$2)*100+$3}'; }
load() { # tool url
  case $1 in
    wrk) pinned 2,3 wrk -t2 -c64 -d${DUR}s "$2" 2>/dev/null | awk '/requests in/ {print $1}' ;;
    h2) pinned 2,3 h2load -t2 -c64 -m 32 -D $DUR "$2" 2>/dev/null | awk '/^requests:/ {print $8}' ;;
    h3) pinned 2,3 tools/perf/h3load/target/release/h3load -t2 -c64 -m 32 -d $DUR "$2" 2>/dev/null | awk '/^requests:/ {print $8}' ;;
  esac
}
dir_of() { [ "$1" = "$A_NAME" ] && echo "$A_DIR" || echo "$B_DIR"; }
pkill -x veil; pkill -x nginx; sleep 1
pinned 2,3 nginx -c $WORK/conf/nginx-up.conf; sleep 1
for P in ${PATHS:-/small.html /}; do
  sz=$([ $P = / ] && echo 54KB || echo 3B)
  for sc in ${SCEN:-h1_file_tls h2_file_tls h1_file_plain h2c_file_plain h3_file h1_proxy_tls h2_proxy_tls l4_tcp}; do
    case $sc in
      h1_file_tls) cfg=veil_file.toml; t=wrk; v=https://127.0.0.1:4443$P ;;
      h2_file_tls) cfg=veil_file.toml; t=h2; v=https://127.0.0.1:4443$P ;;
      h1_file_plain) cfg=veil_file.toml; t=wrk; v=http://127.0.0.1:4080$P ;;
      h2c_file_plain) cfg=veil_file.toml; t=h2; v=http://127.0.0.1:4080$P ;;
      h3_file) cfg=veil_file_h3.toml; t=h3; v=https://127.0.0.1:4443$P ;;
      h1_proxy_tls) cfg=veil_proxy.toml; t=wrk; v=https://127.0.0.1:4443$P ;;
      h2_proxy_tls) cfg=veil_proxy.toml; t=h2; v=https://127.0.0.1:4443$P ;;
      l4_tcp) cfg=veil_l4.toml; t=wrk; v=http://127.0.0.1:4090$P ;;
      *) echo "unknown scenario: $sc" >&2; exit 1 ;;
    esac
    r=1; while [ $r -le $ROUNDS ]; do
      for who in $A_NAME $B_NAME; do
        [ $((r % 2)) = 0 ] && who=$([ $who = $A_NAME ] && echo $B_NAME || echo $A_NAME)
        pkill -x veil; sleep 1
        pinned 0,1 "$(dir_of $who)/veil" -c $WORK/conf/$cfg > /tmp/version-ab-$who.log 2>&1 & vp=$!; sleep 2
        load $t $v > /dev/null  # 暖機（キャッシュ）
        c0=$(cpu $vp); q=$(load $t $v); c1=$(cpu $vp)
        kill $vp; wait $vp 2>/dev/null
        q=${q:-0}; [ "$q" -gt 0 ] 2>/dev/null || q=1
        echo "$sc $sz $who $r $((q/DUR)) $(( (c1-c0)*10000000/q ))"
      done
      r=$((r+1))
    done
  done
done
pkill -x veil; pkill -x nginx
