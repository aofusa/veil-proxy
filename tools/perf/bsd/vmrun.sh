#!/usr/bin/env bash
# ホストから BSD ゲスト（tools/qemu/bsd-vm.sh の VM）へ perf ハーネスを配って実行する薄いラッパ。
# 使い方: bash tools/perf/bsd/vmrun.sh <freebsd|netbsd|openbsd> <x86_64|aarch64> <run_perf_bsd.sh への引数...>
#   （ポートとゲスト内のリポジトリ位置は bsd-vm.sh と同じ既定。PORT / GUEST_ROOT で上書き可）
set -euo pipefail
OS="${1:?os}"; ARCH="${2:?arch}"; shift 2
KEY="${KEY:-$HOME/.ssh/veil_qemu_key}"
case "${OS}-${ARCH}" in
    freebsd-x86_64) base=2310 ;; freebsd-aarch64) base=2320 ;;
    openbsd-x86_64) base=2330 ;; openbsd-aarch64) base=2340 ;;
    netbsd-x86_64)  base=2350 ;; netbsd-aarch64)  base=2360 ;;
    *) echo "unknown os/arch: ${OS} ${ARCH}" >&2; exit 1 ;;
esac
PORT="${PORT:-${base}}"
case "${OS}-${ARCH}" in
    openbsd-*) def_root=/usr/obj/veil-proxy ;;
    netbsd-x86_64) def_root=/work/veil-proxy ;;
    *) def_root=/root/veil-proxy ;;
esac
GUEST_ROOT="${GUEST_ROOT:-${def_root}}"
GUEST="${GUEST:-root@127.0.0.1}"
ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
SSH=(ssh -i "$KEY" -p "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR)

"${SSH[@]}" "$GUEST" "mkdir -p ${GUEST_ROOT}/tools/perf/bsd"
scp -q -i "$KEY" -P "$PORT" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR \
    "$ROOT"/tools/perf/bsd/*.sh "$GUEST:${GUEST_ROOT}/tools/perf/bsd/"
"${SSH[@]}" "$GUEST" "cd ${GUEST_ROOT} && sh tools/perf/bsd/run_perf_bsd.sh $*"
