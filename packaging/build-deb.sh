#!/usr/bin/env bash
# 後方互換ラッパー（packaging/build.sh を呼び出す）
set -euo pipefail
exec "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/build.sh" "$@"