#!/usr/bin/env bash
# .deb と .rpm の両方を Docker コンテナで検証
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "===== Debian/Ubuntu (.deb) test ====="
"${SCRIPT_DIR}/test-deb.sh"

echo
echo "===== Amazon Linux 2023 (.rpm) test ====="
"${SCRIPT_DIR}/test-rpm.sh"

echo
echo "===== All package install tests passed ====="