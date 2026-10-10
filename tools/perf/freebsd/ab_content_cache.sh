#!/bin/sh
# 互換用（F-176）: 本体は tools/perf/bsd/ab_content_cache.sh（FreeBSD / NetBSD / OpenBSD 共通）。
exec sh "$(cd "$(dirname "$0")/../bsd" && pwd)/ab_content_cache.sh" "$@"
