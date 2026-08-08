#!/bin/sh
# F-146 静的コンテンツキャッシュの A/B 計測（FreeBSD ゲスト内で実行）。
#
# 同一バイナリで **設定だけ** を切り替えて比較する（cache off / on）。QEMU/HVF 上の VM は
# 連続計測でスループットが単調劣化するため、各ラウンドで off → on を交互に実行し、
# ラウンド内の比で評価する（順序バイアスを避ける。docs/perf/README.md の教訓参照）。
#
# 使い方: sh tools/perf/freebsd/ab_content_cache.sh [ラウンド数]
set -eu

ROUNDS="${1:-4}"
# NGINX=1 で比較対象 nginx（:5443）も同じラウンド内で計測する。
WITH_NGINX="${NGINX:-0}"
# 2 つの構成は「同じバイナリで config 違い」（既定）と「別バイナリで同じ config」
# （BIN_OFF/BIN_ON を指定）の両方に対応する。後者は release プロファイル（LTO 等）の
# A/B に使う。
BIN="${VEIL_BIN:-/root/veil-proxy/target/release/veil}"
BIN_OFF="${BIN_OFF:-$BIN}"
BIN_ON="${BIN_ON:-$BIN}"
CONF_OFF="${CONF_OFF:-/tmp/veilperf/conf/veil_file.toml}"
CONF_ON="${CONF_ON:-/tmp/veilperf/conf/veil_fc.toml}"
SRV_CPUS="${SRV_CPUS:-0,1}"
GEN_CPUS="${GEN_CPUS:-2,3}"

# h2load の "finished in 1.23s, 45678.90 req/s, ..." から req/s を取り出す。
rps() {
    awk '/finished in/ { for (i = 1; i <= NF; i++) if ($i == "req/s,") { print $(i-1); exit } }'
}

run_one() {
    _cfg="$1"; _url="$2"; _n="$3"; _bin="${4:-$BIN}"
    pkill -x veil >/dev/null 2>&1 || true
    sleep 2
    cpuset -l "${SRV_CPUS}" "${_bin}" -c "${_cfg}" >/dev/null 2>&1 &
    sleep 4
    timeout 40 cpuset -l "${GEN_CPUS}" h2load -t2 -c32 -m16 -n "${_n}" "${_url}" 2>/dev/null | rps
}

if [ "$WITH_NGINX" = "1" ]; then
    nginx -c /tmp/veilperf/conf/nginx.conf >/dev/null 2>&1 || true
    sleep 2
fi

printf 'round\tserver\th2_large_rps\th2_small_rps\n'
r=1
while [ "$r" -le "$ROUNDS" ]; do
    for pair in "off:${CONF_OFF}" "on:${CONF_ON}"; do
        label=${pair%%:*}
        cfg=${pair#*:}
        if [ "$label" = "off" ]; then _b="$BIN_OFF"; else _b="$BIN_ON"; fi
        large=$(run_one "$cfg" "https://127.0.0.1:4443/index.html" 20000 "$_b")
        small=$(run_one "$cfg" "https://127.0.0.1:4443/small.html" 40000 "$_b")
        printf '%s\t%s\t%s\t%s\n' "$r" "veil_cache_$label" "${large:-NA}" "${small:-NA}"
    done
    if [ "$WITH_NGINX" = "1" ]; then
        # nginx は常駐させたまま（veil の起動停止とは独立）。同じラウンド内で測ることで
        # VM の性能劣化ドリフトを veil と同条件で受ける。
        nl=$(timeout 40 cpuset -l "${GEN_CPUS}" h2load -t2 -c32 -m16 -n 20000 \
             https://127.0.0.1:5443/index.html 2>/dev/null | rps)
        ns=$(timeout 40 cpuset -l "${GEN_CPUS}" h2load -t2 -c32 -m16 -n 40000 \
             https://127.0.0.1:5443/small.html 2>/dev/null | rps)
        printf '%s\t%s\t%s\t%s\n' "$r" "nginx" "${nl:-NA}" "${ns:-NA}"
    fi
    r=$((r + 1))
done
pkill -x veil >/dev/null 2>&1 || true
if [ "$WITH_NGINX" = "1" ]; then
    nginx -c /tmp/veilperf/conf/nginx.conf -s quit >/dev/null 2>&1 || true
fi
