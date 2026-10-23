#!/usr/bin/env bash
# .deb を systemd の動くコンテナで試験する（F-251。中身は test-package.sh）
set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEB_FILE="${1:-$(ls -1 "${SCRIPT_DIR}/../output"/veil_*_amd64.deb | head -1)}"
"${SCRIPT_DIR}/test-package.sh" deb "${VEIL_TEST_BASE:-ubuntu:24.04}" "$DEB_FILE"
