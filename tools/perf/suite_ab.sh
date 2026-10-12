#!/usr/bin/env bash
# =============================================================================
# suite_ab.sh — run_perf.sh のフルスイートを 2 つのイメージで構成ごとに交互に回す（退行確認）
# =============================================================================
#
# run_perf.sh はビルドを外側のループにするので、`BUILDS="base new"` では base の全構成（約 4.5 時間）の
# あとに new の全構成を測ることになり、時間をまたいだ比較になる（同一イメージでもラン間で 15% 振れる。
# docs/perf/README.md）。ここでは構成 1 つごとに run_perf.sh を呼び、base と new を続けて測る。
# 実行順の偏りを打ち消すため、構成ごとに順番を入れ替える（偶数番目は base → new、奇数番目は new → base）。
#
# 使い方:
#   docker build -f docker/Dockerfile.glibc -t veil:glibc .                  # 新しい側
#   git worktree add /tmp/v070 v0.7.0 && (cd /tmp/v070 && \
#       docker build -f docker/Dockerfile.glibc -t veil:v070 .)             # 比較対象
#   BASE=v070 NEW=glibc bash tools/perf/suite_ab.sh
#   python3 tools/perf/suite_ab_summary.py tools/perf/results/suite_ab/<日時>.tsv v070 glibc
#
# 主な環境変数:
#   BASE / NEW（イメージ veil:<名前> のタグ。結果の target 列は veil_<名前>）
#   CONFIG_GLOB（既定 *。run_perf.sh と同じ）  ITERATIONS（既定 3）
#   NGINX（1 で最初に nginx のベースラインも 1 回測る。既定 1）
#   OUT（結果 TSV。既定 tools/perf/results/suite_ab/<日時>.tsv）
#
# 出力は run_perf.sh の results_raw.tsv と同じ 11 列（analyze_results.sh でも集計できる）。
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
BASE="${BASE:?BASE（比較対象のイメージタグ）を指定する}"
NEW="${NEW:?NEW（新しい側のイメージタグ）を指定する}"
CONFIG_GLOB="${CONFIG_GLOB:-*}"
NGINX="${NGINX:-1}"
export ITERATIONS="${ITERATIONS:-3}"
OUT="${OUT:-$HERE/results/suite_ab/$(date +%Y%m%d-%H%M%S).tsv}"
mkdir -p "$(dirname "$OUT")"
RAW="$HERE/results/results_raw.tsv"

for t in "$BASE" "$NEW"; do
    docker image inspect "veil:$t" >/dev/null 2>&1 || { echo "veil:$t が無い" >&2; exit 1; }
done

log() { echo "[$(date +%H:%M:%S)] $*"; }

if [ "$NGINX" = "1" ]; then
    log "nginx ベースライン"
    BUILDS="" bash "$HERE/run_perf.sh" > "$OUT.nginx.log" 2>&1
    cp "$RAW" "$OUT"
else
    head -1 "$RAW" > "$OUT" 2>/dev/null || \
        printf 'target\tconfig\tproto\titeration\treq_per_sec\ttransfer\tlat_avg\tlat_p99\tnon2xx\tcpu_pct\tmem_mb\n' > "$OUT"
fi

i=0
for cfgfile in "$HERE"/configs/$CONFIG_GLOB.toml; do
    [ -e "$cfgfile" ] || continue
    name=$(basename "$cfgfile" .toml)
    case "$name" in *_alloc) continue ;; esac
    if [ $((i % 2)) = 0 ]; then order="$BASE $NEW"; else order="$NEW $BASE"; fi
    i=$((i + 1))
    log "$i: $name ($order)"
    SKIP_NGINX=1 CONFIGS="$name" BUILDS="$order" bash "$HERE/run_perf.sh" >> "$OUT.log" 2>&1
    tail -n +2 "$RAW" >> "$OUT"
done
log "完了: $OUT"
