#!/usr/bin/env bash
# =============================================================================
# alloc_soak.sh — 長時間運用の RSS 安定性（ソーク）試験（F-183）
# =============================================================================
#
# veil を 1 プロセス起動したまま、負荷（全種をランダムな順・接続数で）とアイドルを
# 交互に繰り返し、RSS を一定間隔で採取する。24 時間 365 日の運用で問題になる
# 「負荷の山を越えるたびに RSS の底が上がっていく」（断片化・保持の蓄積）を検出する。
#
# 判定（終了時に表示）:
#   - 失敗要求の合計（0 であること）
#   - 各アイドル区間の末尾の RSS（= 底）の推移と、その回帰直線の傾き（MB/時）
#   - 前半と後半の底の平均の差
#
# 使い方:
#   BIN=/path/to/veil MINUTES=60 bash tools/perf/alloc_soak.sh [ENV=VAL ...] [OPT:<key>=<value> ...]
#
# 環境変数: BIN（必須） MINUTES (30) LOAD_SECS (20) IDLE_SECS (15) SAMPLE_SECS (2) SEED (42)
#           THREADS (2) VEIL_CPUS (0,1) LOAD_CPUS (2,3) OUT（RSS 時系列 TSV）
# alloc_ab.sh と同じ作業ディレクトリ・設定・上流（nginx）を使う。
# =============================================================================
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="${BIN:?BIN（veil バイナリ）を指定してください}"
MINUTES="${MINUTES:-30}"
LOAD_SECS="${LOAD_SECS:-20}"
IDLE_SECS="${IDLE_SECS:-15}"
SAMPLE_SECS="${SAMPLE_SECS:-2}"
THREADS="${THREADS:-2}"
VEIL_CPUS="${VEIL_CPUS:-0,1}"
LOAD_CPUS="${LOAD_CPUS:-2,3}"
TLS_PORT=19443
H2C_PORT=19080
UP_PORT=19081
H2_IMG=local/h2load:latest
WRK_IMG=williamyeh/wrk:latest
WRK_OSSL_IMG=local/wrk-openssl:latest
WORK="$SCRIPT_DIR/results/alloc_ab/work"
STAMP="$(date +%Y%m%d-%H%M%S)"
OUT="${OUT:-$SCRIPT_DIR/results/alloc_ab/soak-$STAMP.tsv}"
UP_NAME=alloc-soak-upstream

log() { printf '[alloc_soak] %s\n' "$*" >&2; }

[ -f "$WORK/veil.toml" ] || { log "先に alloc_ab.sh を 1 回実行して $WORK を作ること"; exit 1; }
mkdir -p "$(dirname "$OUT")"

envs=() opts=()
for a in "$@"; do
    case "$a" in
        OPT:*) opts+=(-o "${a#OPT:}") ;;
        *) envs+=("$a") ;;
    esac
done

docker rm -f "$UP_NAME" >/dev/null 2>&1
docker run -d --rm --network host --cpuset-cpus "${LOAD_CPUS##*,}" \
    -v "$WORK/nginx-up.conf:/etc/nginx/nginx.conf:ro" -v "$WORK/www:/var/www:ro" \
    --name "$UP_NAME" nginx:alpine >/dev/null

env "${envs[@]}" taskset -c "$VEIL_CPUS" "$BIN" -c "$WORK/veil.toml" "${opts[@]}" >"$WORK/soak-veil.log" 2>&1 &
VPID=$!
cleanup() {
    kill "$VPID" 2>/dev/null
    docker rm -f "$UP_NAME" >/dev/null 2>&1
}
trap cleanup EXIT
trap 'cleanup; exit 130' INT TERM
sleep 3
kill -0 "$VPID" 2>/dev/null || { log "veil が起動しない"; exit 1; }

printf 'elapsed_s\tphase\trss_kb\n' > "$OUT"
T0=$(date +%s)
PHASE=idle
rss() { awk '/^VmRSS:/ {print $2}' "/proc/$VPID/status" 2>/dev/null || echo 0; }
sample_until() {
    local end=$1
    while [ "$(date +%s)" -lt "$end" ]; do
        printf '%s\t%s\t%s\n' "$(( $(date +%s) - T0 ))" "$PHASE" "$(rss)" >> "$OUT"
        sleep "$SAMPLE_SECS"
    done
}

# 負荷の候補（毎サイクルいずれか 1〜2 種を同時に掛ける）
loads=(
    "h2load --h1 -t2 -c%C https://127.0.0.1:$TLS_PORT/"
    "h2load --h1 -t2 -c%C https://127.0.0.1:$TLS_PORT/3b.html"
    "h2load -t2 -c%c -m16 https://127.0.0.1:$TLS_PORT/3b.html"
    "h2load --h1 -t2 -c%C https://127.0.0.1:$TLS_PORT/p/3b.html"
    "h2load -t2 -c%c -m8 http://127.0.0.1:$H2C_PORT/p/"
    "wrk -t2 -c%c https://127.0.0.1:$TLS_PORT/3b.html"
    "wrkossl -t2 -c%c https://127.0.0.1:$TLS_PORT/"
)

# 負荷列の乱数の種（同じ SEED ならバリアント間で同じ負荷列になる）
RANDOM="${SEED:-42}"
FAILED=0
cycle=0
END=$(( T0 + MINUTES * 60 ))
FLOORS=()
while [ "$(date +%s)" -lt "$END" ]; do
    cycle=$((cycle + 1))
    PHASE=load
    pids=()
    nload=$(( RANDOM % 2 + 1 ))
    for _ in $(seq 1 "$nload"); do
        spec="${loads[$(( RANDOM % ${#loads[@]} ))]}"
        big=$(( (RANDOM % 4 + 1) * 64 ))     # 64〜256 接続
        small=$(( (RANDOM % 4 + 1) * 8 ))    # 8〜32 接続
        spec="${spec//%C/$big}"; spec="${spec//%c/$small}"
        read -r tool args <<<"$spec"
        logf="$WORK/soak-load-$cycle-$RANDOM.log"
        case "$tool" in
            h2load) docker run --rm --network host --cpuset-cpus "$LOAD_CPUS" --entrypoint h2load "$H2_IMG" \
                        -D "$LOAD_SECS" $args >"$logf" 2>&1 & ;;
            wrk) docker run --rm --network host --cpuset-cpus "$LOAD_CPUS" "$WRK_IMG" \
                        -d "${LOAD_SECS}s" -H "Connection: close" $args >"$logf" 2>&1 & ;;
            wrkossl) docker run --rm --network host --cpuset-cpus "$LOAD_CPUS" "$WRK_OSSL_IMG" \
                        -d "${LOAD_SECS}s" $args >"$logf" 2>&1 & ;;
        esac
        pids+=("$!:$tool:$logf")
    done
    sample_until $(( $(date +%s) + LOAD_SECS + 2 ))
    for p in "${pids[@]}"; do
        IFS=: read -r pid tool logf <<<"$p"
        wait "$pid" 2>/dev/null
        if [ "$tool" = h2load ]; then
            f=$(awk '/^requests:/ {f=$10+$12} /^status codes:/ {s=$7+$9} END {print f+s+0}' "$logf")
        else
            f=$(awk '/Socket errors:/ {gsub(",","");f=$4+$6+$8+$10} /Non-2xx or 3xx/ {s=$5} END {print f+s+0}' "$logf")
        fi
        FAILED=$(( FAILED + ${f:-0} ))
        # 失敗したサイクルの負荷ログと veil のログ末尾を残す（原因の切り分け用）
        if [ "${f:-0}" -gt 0 ]; then
            { echo "== cycle $cycle: $tool"; cat "$logf"; echo "-- veil.log tail";
              tail -20 "$WORK/soak-veil.log"; } >> "$OUT.failures"
        fi
        rm -f "$logf"
    done
    kill -0 "$VPID" 2>/dev/null || { log "!! veil が終了した（cycle $cycle）"; FAILED=$((FAILED + 1)); break; }
    PHASE=idle
    sample_until $(( $(date +%s) + IDLE_SECS ))
    FLOORS+=("$(( $(date +%s) - T0 )):$(rss)")
    log "cycle $cycle 底 RSS=$(rss)KB 失敗累計=$FAILED"
done

printf '%s\n' "${FLOORS[@]}" > "$OUT.floors"
python3 - "$OUT.floors" "$FAILED" "$OUT" <<'PY'
import sys
pts = [tuple(map(float, l.split(':'))) for l in open(sys.argv[1]) if l.strip()]
failed = int(sys.argv[2])
rss = [float(l.split('\t')[2]) for l in open(sys.argv[3]).read().splitlines()[1:] if l.count('\t') == 2]
n = len(pts)
xs = [p[0] / 3600 for p in pts]
ys = [p[1] / 1024 for p in pts]
mx, my = sum(xs) / n, sum(ys) / n
slope = sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / max(sum((x - mx) ** 2 for x in xs), 1e-9)
h = n // 2
print(f"cycles={n} failed_requests={failed}")
print(f"rss_max={max(rss)/1024:.1f}MB floor_first={ys[0]:.1f}MB floor_last={ys[-1]:.1f}MB")
print(f"floor_mean_first_half={sum(ys[:h])/max(h,1):.1f}MB floor_mean_second_half={sum(ys[h:])/max(n-h,1):.1f}MB")
print(f"floor_slope={slope:+.2f}MB/h")
# 立ち上がり（プール・キャッシュが最大負荷の規模まで埋まる）を除いた定常状態の傾き。
# 24 時間運用で効くのはこちら（後半の傾きが 0 付近なら底は頭打ち）。
xs2, ys2 = xs[h:], ys[h:]
if len(xs2) >= 2:
    mx2, my2 = sum(xs2) / len(xs2), sum(ys2) / len(ys2)
    s2 = sum((x - mx2) * (y - my2) for x, y in zip(xs2, ys2)) / max(sum((x - mx2) ** 2 for x in xs2), 1e-9)
    print(f"floor_slope_second_half={s2:+.2f}MB/h")
PY
