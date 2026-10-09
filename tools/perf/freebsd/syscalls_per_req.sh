#!/bin/sh
# 互換用（F-176）: 本体は tools/perf/bsd/syscalls_per_req.sh（FreeBSD / NetBSD / OpenBSD 共通）。
exec sh "$(cd "$(dirname "$0")/../bsd" && pwd)/syscalls_per_req.sh" "$@"
