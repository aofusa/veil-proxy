#!/bin/sh
# =============================================================================
# cpu_per_req.sh — 全シナリオの「サーバの CPU 時間 / 要求」と rps を veil・nginx で交互に測る（BSD、v0.8.0）
# =============================================================================
#
# run_perf_bsd.sh を一度実行して ${WORK}（既定 /tmp/veilperf）に設定・配信ファイルがある状態で使う。
# 計測対象（veil・nginx）は 0,1 コア、負荷生成と上流（nginx-up）は 2,3 コアに固定する。
# VM ではクライアントやホストが律速になって rps が並ぶことが多い（h2c・HTTP/3 の 54KB 等）ので、
# サーバの効率は `ps -o time` の差分を要求数で割った値（ns/要求）で比べる。各シナリオ 3 回、
# 回ごとに veil と nginx の順を入れ替える。nginx の CPU は計測対象のワーカーだけ（上流は別プロセス）。
#
# 出力: scenario size server round rps cpu_ns_per_req
# 使い方（BSD ゲスト内・root）: sh tools/perf/bsd/cpu_per_req.sh [DUR=秒]
# =============================================================================
set -u
. "$(cd "$(dirname "$0")" && pwd)/lib.sh"
REPO="${REPO:-$(cd "$(dirname "$0")/../../.." && pwd)}"
cd "$REPO"
WORK="${WORK:-/tmp/veilperf}"; DUR=${DUR:-8}
cpu() { ps -o time= -p "$1" | awk -F'[:.]' '{print ($1*60+$2)*100+$3}'; }
cpusum() { t=0; for p in $1; do c=$(cpu $p); t=$((t+c)); done; echo $t; }
load() { # tool url
  case $1 in
    wrk) pinned 2,3 wrk -t2 -c64 -d${DUR}s "$2" 2>/dev/null | awk '/requests in/ {print $1}' ;;
    h2) pinned 2,3 h2load -t2 -c64 -m 32 -D $DUR "$2" 2>/dev/null | awk '/^requests:/ {print $8}' ;;
    h3) pinned 2,3 tools/perf/h3load/target/release/h3load -t2 -c64 -m 32 -d $DUR "$2" 2>/dev/null | awk '/^requests:/ {print $8}' ;;
  esac
}
pkill -x veil; pkill -x nginx; sleep 1
pinned 2,3 nginx -c $WORK/conf/nginx-up.conf; pinned 0,1 nginx -c $WORK/conf/nginx.conf; sleep 1
mp=$(cat $WORK/nginx/nginx.pid); npids=$(pgrep -P $mp | tr '\n' ' ')
for P in /small.html /; do
  sz=$([ $P = / ] && echo 54KB || echo 3B)
  for sc in h1_file_tls h2_file_tls h1_file_plain h2c_file_plain h3_file h1_proxy_tls h2_proxy_tls l4_tcp; do
    case $sc in
      h1_file_tls) cfg=veil_file.toml; t=wrk; v=https://127.0.0.1:4443$P; n=https://127.0.0.1:5443$P ;;
      h2_file_tls) cfg=veil_file.toml; t=h2; v=https://127.0.0.1:4443$P; n=https://127.0.0.1:5443$P ;;
      h1_file_plain) cfg=veil_file.toml; t=wrk; v=http://127.0.0.1:4080$P; n=http://127.0.0.1:5080$P ;;
      h2c_file_plain) cfg=veil_file.toml; t=h2; v=http://127.0.0.1:4080$P; n=http://127.0.0.1:5080$P ;;
      h3_file) cfg=veil_file_h3.toml; t=h3; v=https://127.0.0.1:4443$P; n=https://127.0.0.1:5443$P ;;
      h1_proxy_tls) cfg=veil_proxy.toml; t=wrk; v=https://127.0.0.1:4443$P; n=https://127.0.0.1:5443/proxy$P ;;
      h2_proxy_tls) cfg=veil_proxy.toml; t=h2; v=https://127.0.0.1:4443$P; n=https://127.0.0.1:5443/proxy$P ;;
      l4_tcp) cfg=veil_l4.toml; t=wrk; v=http://127.0.0.1:4090$P; n=http://127.0.0.1:5090$P ;;
    esac
    for r in 1 2 3; do
      for who in veil nginx; do
        [ $((r % 2)) = 0 ] && who=$([ $who = veil ] && echo nginx || echo veil)
        if [ $who = veil ]; then
          pkill -x veil; sleep 1
          pinned 0,1 target/release/veil -c $WORK/conf/$cfg > /tmp/ca.log 2>&1 & vp=$!; sleep 2
          load $t $v > /dev/null  # 暖機（キャッシュ）
          c0=$(cpu $vp); q=$(load $t $v); c1=$(cpu $vp)
          kill $vp; wait $vp 2>/dev/null
        else
          load $t $n > /dev/null
          c0=$(cpusum "$npids"); q=$(load $t $n); c1=$(cpusum "$npids")
        fi
        q=${q:-0}; [ "$q" -gt 0 ] 2>/dev/null || q=1
        echo "$sc $sz $who $r $((q/DUR)) $(( (c1-c0)*10000000/q ))"
      done
    done
  done
done
pkill -x nginx
