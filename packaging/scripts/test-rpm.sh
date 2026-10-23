#!/usr/bin/env bash
# .rpm を systemd の動くコンテナで試験する（F-251。中身は test-package.sh）
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RPM_FILE="${1:-$(ls -1 "${SCRIPT_DIR}/../output"/veil-*.x86_64.rpm | head -1)}"
"${SCRIPT_DIR}/test-package.sh" rpm "${VEIL_TEST_BASE:-amazonlinux:2023}" "$RPM_FILE"
