#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TAG="${1:-latest}"

cd "${ROOT_DIR}"

echo "=== Building glibc image (distroless) ==="
docker build -f docker/Dockerfile.glibc \
    -t "veil:glibc" \
    -t "veil:${TAG}" \
    .

echo "=== Building musl image (scratch) ==="
docker build -f docker/Dockerfile.musl \
    -t "veil:musl" \
    .

echo "Done."
echo "  veil:glibc"
echo "  veil:musl"
echo "  veil:${TAG} (glibc alias)"
