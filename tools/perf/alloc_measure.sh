#!/usr/bin/env bash
# 1 リクエストあたりのヒープアロケーション数を実測する（F-165 Phase 1）。
#
# `alloc-stats` feature 付きでビルドしたイメージ（例:
# `docker build -f docker/Dockerfile.glibc --build-arg CARGO_FEATURES=full-container,alloc-stats
#   -t veil:alloc-container ..`）を `h2c_proxy_lab.sh` で起動し、
# Prometheus の `veil_alloc_*` ゲージを負荷の前後でスクレイプして差分を取る。
#
# 使い方:
#   bash tools/perf/alloc_measure.sh [config] [image] [path] [requests]
#     config   : tools/perf/configs/<config>.toml（既定 h2c_proxy_alloc）
#     image    : alloc-stats 付きイメージ（既定 veil:alloc-container）
#     path     : 負荷をかけるパス（既定 /）
#     requests : リクエスト数（既定 20000）
#
# 出力: allocs/req, deallocs/req, reallocs/req, bytes/req
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CFG="${1:-h2c_proxy_alloc}"
IMG="${2:-veil:alloc-container}"
LOAD_PATH="${3:-/}"
NREQ="${4:-20000}"
NET=veil-perf-lab
VEIL_NAME=lab-veil
H2_IMG=local/h2load:latest
METRICS_PATH="${METRICS_PATH:-/__metrics}"

bash "$SCRIPT_DIR/h2c_proxy_lab.sh" up "$CFG" "$IMG" || exit 1

# ウォームアップ（接続確立・プール充填・遅延初期化を計測窓の外へ出す）
docker run --rm --network "$NET" --entrypoint h2load "$H2_IMG" \
    -n 3000 -c 100 -m 10 "http://$VEIL_NAME:8080$LOAD_PATH" >/dev/null 2>&1

scrape() {
    docker run --rm --network "$NET" --entrypoint h2load "$H2_IMG" \
        -n 1 -c 1 "http://$VEIL_NAME:8080$METRICS_PATH" >/dev/null 2>&1
    docker run --rm --network "$NET" curlimages/curl:latest -s \
        --http2-prior-knowledge "http://$VEIL_NAME:8080$METRICS_PATH" 2>/dev/null \
        | awk '/^veil_alloc_/ {print $1"="$2}'
}

before="$(scrape)"
[ -z "$before" ] && { echo "!! metrics を取得できない（[prometheus] enabled と alloc-stats ビルドを確認）" >&2; exit 1; }

docker run --rm --network "$NET" --entrypoint h2load "$H2_IMG" \
    -n "$NREQ" -c 100 -m 10 "http://$VEIL_NAME:8080$LOAD_PATH" > "$SCRIPT_DIR/results/lab/alloc_load.log" 2>&1
done_req=$(awk '/^requests:/ {print $4}' "$SCRIPT_DIR/results/lab/alloc_load.log")
after="$(scrape)"

echo "== h2load =="
grep -E 'finished in|^requests:' "$SCRIPT_DIR/results/lab/alloc_load.log"
echo "== allocations / request（完了 ${done_req} リクエスト、スクレイプ 2 回分を含む） =="
python3 - "$done_req" <<PY
import sys
req = float(sys.argv[1] or 1)
def parse(s):
    d = {}
    for line in s.strip().splitlines():
        k, _, v = line.partition('=')
        try:
            d[k] = float(v)
        except ValueError:
            pass
    return d
before = parse('''$before''')
after = parse('''$after''')
for k in sorted(after):
    delta = after[k] - before.get(k, 0.0)
    print(f"{k:34s} {delta:14.0f}  {delta/req:10.3f}/req")
PY
