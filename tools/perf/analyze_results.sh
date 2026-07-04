#!/usr/bin/env bash
# 反復計測の生データ（results_raw.tsv）を (target, config, proto) 単位で集計し、
# Req/s の median±stdev・レイテンシ/CPU/メモリの median・エラー合計を Markdown 表で出力する。
# docs/artifacts/perf_benchmark/analyze_results.py の shell 版（tools/perf は shell のみ）。
#
# 使い方: bash tools/perf/analyze_results.sh [results_raw.tsv]
set -euo pipefail

RAW="${1:-$(cd "$(dirname "$0")" && pwd)/results/results_raw.tsv}"
[ -f "$RAW" ] || { echo "raw tsv が見つかりません: $RAW" >&2; exit 1; }

awk -F'\t' '
function parse_val(s,   v) {
    if (s == "NA" || s == "") return "NA"
    v = s
    gsub(/GB\/s|MB\/s|KB\/s|B\/s/, "", v)
    gsub(/ms|us|%/, "", v)
    gsub(/s$/, "", v)
    if (v ~ /^-?[0-9]+(\.[0-9]+)?$/) return v + 0
    return "NA"
}
# 数値配列 a[1..n] の median（挿入ソート。n は反復数で小）
function median(a, n,   i, j, key, m) {
    for (i = 2; i <= n; i++) { key = a[i]; j = i - 1
        while (j >= 1 && a[j] > key) { a[j+1] = a[j]; j-- }
        a[j+1] = key }
    if (n == 0) return 0
    m = int(n/2)
    if (n % 2) return a[m+1]
    return (a[m] + a[m+1]) / 2.0
}
function stdev(a, n,   i, mean, s) {
    if (n < 2) return 0
    mean = 0; for (i = 1; i <= n; i++) mean += a[i]; mean /= n
    s = 0; for (i = 1; i <= n; i++) s += (a[i]-mean)*(a[i]-mean)
    return sqrt(s / (n - 1))
}
NR == 1 { next }   # ヘッダ行
{
    key = $1 "\t" $2 "\t" $3
    if (!(key in seen)) { seen[key] = 1; order[++nk] = key }
    reqps = parse_val($5); lat = parse_val($7); cpu = parse_val($10); mem = parse_val($11); err = parse_val($9)
    if (reqps != "NA") { c_r[key]++; R[key, c_r[key]] = reqps }
    if (lat   != "NA") { c_l[key]++; L[key, c_l[key]] = lat }
    if (cpu   != "NA") { c_c[key]++; C[key, c_c[key]] = cpu }
    if (mem   != "NA") { c_m[key]++; M[key, c_m[key]] = mem }
    if (err   != "NA") { esum[key] += err }
}
END {
    print "| Target | Config | Proto | Req/s (Median ± Stdev) | Latency Avg (Median) | CPU% (Median) | Mem MB (Median) | Errors |"
    print "|---|---|---|---|---|---|---|---|"
    for (i = 1; i <= nk; i++) {
        key = order[i]
        nr = c_r[key]; for (j = 1; j <= nr; j++) ar[j] = R[key, j]
        nl = c_l[key]; for (j = 1; j <= nl; j++) al[j] = L[key, j]
        ncc = c_c[key]; for (j = 1; j <= ncc; j++) ac[j] = C[key, j]
        nm = c_m[key]; for (j = 1; j <= nm; j++) am[j] = M[key, j]
        rm = median(ar, nr); rs = stdev(ar, nr)
        lm = (nl ? median(al, nl) : 0)
        cm = (ncc ? median(ac, ncc) : 0)
        mm = (nm ? median(am, nm) : 0)
        split(key, kk, "\t")
        printf "| %s | %s | %s | %.1f ± %.1f | %.2fms | %.1f%% | %.1fMB | %.0f |\n", \
            kk[1], kk[2], kk[3], rm, rs, lm, cm, mm, esum[key]
    }
}
' "$RAW"
