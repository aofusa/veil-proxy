#!/usr/bin/env bash
# ホスト（macOS）から FreeBSD ゲストへ perf ハーネスを配って実行する薄いラッパ。
# 使い方: bash tools/perf/freebsd/vmrun.sh <run_perf_freebsd.sh への引数...>
set -euo pipefail
KEY="${KEY:-$HOME/.ssh/veil_qemu_key}"
PORT="${PORT:-2320}"
GUEST="${GUEST:-root@127.0.0.1}"
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"

ssh -i "$KEY" -p "$PORT" "$GUEST" "mkdir -p /root/veil-proxy/tools/perf/freebsd"
scp -q -i "$KEY" -P "$PORT" "$ROOT/tools/perf/freebsd/run_perf_freebsd.sh" \
    "$GUEST:/root/veil-proxy/tools/perf/freebsd/"
ssh -i "$KEY" -p "$PORT" "$GUEST" \
    "cd /root/veil-proxy && sh tools/perf/freebsd/run_perf_freebsd.sh $*"
