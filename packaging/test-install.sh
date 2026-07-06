#!/usr/bin/env bash
# .deb と .rpm の両方を Docker コンテナで検証
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "===== Debian/Ubuntu (.deb) test ====="
"${ROOT}/test-docker-install.sh"

echo
echo "===== Amazon Linux 2023 (.rpm) test ====="
"${ROOT}/test-docker-install-rpm.sh"

echo
echo "===== All package install tests passed ====="