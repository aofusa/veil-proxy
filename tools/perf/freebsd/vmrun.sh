#!/usr/bin/env bash
# 互換用（F-176）: 本体は tools/perf/bsd/vmrun.sh <os> <arch> ...。従来どおり FreeBSD の
# aarch64 ゲスト（ポート 2320）が既定（ARCH=x86_64 で amd64 ゲスト）。
exec bash "$(cd "$(dirname "$0")/../bsd" && pwd)/vmrun.sh" freebsd "${ARCH:-aarch64}" "$@"
