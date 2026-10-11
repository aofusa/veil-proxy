#!/bin/sh
# =============================================================================
# alloc_soak_bsd.sh — BSD ゲスト内の長時間 RSS 安定性（ソーク）試験（F-183）
# =============================================================================
#
# tools/perf/alloc_soak.sh の BSD 版。veil を 1 プロセス起動したまま、負荷（全種を順に、
# 接続数を変えながら）とアイドルを交互に繰り返し、各アイドル区間の末尾の RSS（= 底）と
# 失敗要求数を記録する。負荷の山を越えるたびに底が上がっていかないことを確かめる。
#
# 事前に tools/perf/bsd/alloc_ab_bsd.sh を 1 回実行して作業ディレクトリ（設定・証明書・
# 配信ファイル）を作っておくこと。
#
# 使い方: BIN=./target/release/veil MINUTES=40 sh tools/perf/bsd/alloc_soak_bsd.sh [ENV=VAL ...]
# 出力: 各サイクルの「経過秒 底RSS(KB) 失敗累計」と、最後に前半/後半の底の平均・傾き
# =============================================================================
set -u

BIN="${BIN:?BIN を指定してください}"
MINUTES="${MINUTES:-40}"
LOAD_SECS="${LOAD_SECS:-20}"
IDLE_SECS="${IDLE_SECS:-15}"
VEIL_CPUS="${VEIL_CPUS:-0,1}"
LOAD_CPUS="${LOAD_CPUS:-2,3}"
WORK="${WORK:-/tmp/alloc_ab_bsd}"
OUT="${OUT:-$WORK/soak-$(date +%Y%m%d-%H%M%S).txt}"
TLS=https://127.0.0.1:19443
H2C=http://127.0.0.1:19080

[ -f "$WORK/veil.toml" ] || { echo "先に alloc_ab_bsd.sh を実行すること" >&2; exit 1; }
nginx -c "$WORK/nginx-up.conf" 2>/dev/null
# shellcheck disable=SC2086
env "$@" cpuset -l "$VEIL_CPUS" "$BIN" -c "$WORK/veil.toml" > "$WORK/soak-veil.log" 2>&1 &
VPID=$!
cleanup() { kill "$VPID" 2>/dev/null; kill "$(cat "$WORK/nginx.pid" 2>/dev/null)" 2>/dev/null; }
trap cleanup EXIT
trap 'cleanup; exit 130' INT TERM
sleep 3

rss() { ps -o rss= -p "$VPID" | tr -d ' '; }
: > "$OUT"
T0=$(date +%s)
END=$((T0 + MINUTES * 60))
FAILED=0
cycle=0
while [ "$(date +%s)" -lt "$END" ]; do
    cycle=$((cycle + 1))
    c=$(( (cycle % 4 + 1) * 64 ))
    s=$(( (cycle % 4 + 1) * 8 ))
    case $((cycle % 6)) in
        0) cmd="wrk -t2 -c$c -d${LOAD_SECS}s $TLS/" ;;
        1) cmd="wrk -t2 -c$c -d${LOAD_SECS}s $TLS/3b.html" ;;
        2) cmd="h2load -t2 -c$s -m16 -D $LOAD_SECS $TLS/3b.html" ;;
        3) cmd="wrk -t2 -c$c -d${LOAD_SECS}s $TLS/p/3b.html" ;;
        4) cmd="h2load -t2 -c$s -m8 -D $LOAD_SECS $H2C/p/" ;;
        5) cmd="wrk -t2 -c$s -d${LOAD_SECS}s -H Connection:close $TLS/3b.html" ;;
    esac
    # shellcheck disable=SC2086
    cpuset -l "$LOAD_CPUS" $cmd > "$WORK/soak-load.log" 2>&1
    case "$cmd" in
        wrk*) f=$(awk '/Socket errors:/ {gsub(",","");f=$4+$6+$8+$10} /Non-2xx or 3xx/ {s=$5} END {print f+s+0}' "$WORK/soak-load.log") ;;
        *) f=$(awk '/^requests:/ {f=$10+$12} /^status codes:/ {s=$7+$9} END {print f+s+0}' "$WORK/soak-load.log") ;;
    esac
    FAILED=$((FAILED + f))
    # 失敗したサイクルの負荷ログと veil のログ末尾を残す（原因の切り分け用）
    if [ "${f:-0}" -gt 0 ]; then
        { echo "== cycle $cycle: $cmd"; cat "$WORK/soak-load.log"; echo "-- veil.log tail";
          tail -20 "$WORK/soak-veil.log"; } >> "$OUT.failures"
    fi
    kill -0 "$VPID" 2>/dev/null || { echo "!! veil が終了した（cycle $cycle）" >&2; FAILED=$((FAILED + 1)); break; }
    sleep "$IDLE_SECS"
    echo "$(( $(date +%s) - T0 )) $(rss) $FAILED" | tee -a "$OUT"
done

awk -v failed="$FAILED" '
{ x[NR] = $1 / 3600; y[NR] = $2 / 1024; n = NR; if ($2 > max) max = $2 }
END {
    for (i = 1; i <= n; i++) { sx += x[i]; sy += y[i] }
    mx = sx / n; my = sy / n
    for (i = 1; i <= n; i++) { sxy += (x[i] - mx) * (y[i] - my); sxx += (x[i] - mx) ^ 2 }
    h = int(n / 2)
    for (i = 1; i <= h; i++) a += y[i]
    for (i = h + 1; i <= n; i++) b += y[i]
    printf "cycles=%d failed_requests=%d floor_max=%.1fMB floor_first=%.1fMB floor_last=%.1fMB\n", n, failed, max / 1024, y[1], y[n]
    printf "floor_mean_first_half=%.1fMB floor_mean_second_half=%.1fMB floor_slope=%+.2fMB/h\n", a / h, b / (n - h), (sxx > 0 ? sxy / sxx : 0)
    # 立ち上がりを除いた定常状態（後半）の傾き
    m = n - h; sx = 0; sy = 0; sxy = 0; sxx = 0
    for (i = h + 1; i <= n; i++) { sx += x[i]; sy += y[i] }
    mx = sx / m; my = sy / m
    for (i = h + 1; i <= n; i++) { sxy += (x[i] - mx) * (y[i] - my); sxx += (x[i] - mx) ^ 2 }
    printf "floor_slope_second_half=%+.2fMB/h\n", (sxx > 0 ? sxy / sxx : 0)
}' "$OUT"
