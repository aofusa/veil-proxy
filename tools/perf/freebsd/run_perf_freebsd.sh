#!/bin/sh
# 互換用（F-176）: 本体は tools/perf/bsd/run_perf_bsd.sh（FreeBSD / NetBSD / OpenBSD 共通）。
exec sh "$(cd "$(dirname "$0")/../bsd" && pwd)/run_perf_bsd.sh" "$@"
