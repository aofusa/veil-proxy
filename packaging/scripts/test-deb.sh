#!/usr/bin/env bash
# Ubuntu Docker コンテナ内で .deb パッケージのインストールと動作確認
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
DOCKER_DIR="${SCRIPT_DIR}/docker"
OUTPUT_DIR="${ROOT}/packaging/output"
DEB_FILE="$(ls -1 "${OUTPUT_DIR}"/veil_*.deb | head -1)"
IMAGE="${VEIL_TEST_IMAGE:-veil-package-test:ubuntu24.04}"
CONTAINER_NAME="veil-package-test-$$"

if [ ! -f "${DEB_FILE}" ]; then
    echo "ERROR: deb package not found. Run packaging/scripts/build.sh first." >&2
    exit 1
fi

cleanup() {
    docker rm -f "${CONTAINER_NAME}" >/dev/null 2>&1 || true
}
trap cleanup EXIT

if ! docker image inspect "${IMAGE}" >/dev/null 2>&1; then
    echo "==> Building test image (${IMAGE})"
    docker build -t "${IMAGE}" -f "${DOCKER_DIR}/Dockerfile.test-deb" "${DOCKER_DIR}"
fi

echo "==> Starting Ubuntu container with systemd (${IMAGE})"
docker run -d --privileged --name "${CONTAINER_NAME}" \
    --cgroupns=host \
    -v /sys/fs/cgroup:/sys/fs/cgroup:rw \
    "${IMAGE}"

echo "==> Waiting for container init"
for _ in $(seq 1 60); do
    if docker exec "${CONTAINER_NAME}" systemctl is-system-running --wait 2>/dev/null | grep -qE 'running|degraded'; then
        break
    fi
    sleep 2
done

echo "==> Installing package"
cat "${DEB_FILE}" | docker exec -i "${CONTAINER_NAME}" sh -c 'cat > /tmp/veil.deb'
docker exec "${CONTAINER_NAME}" bash -c '
    set -euo pipefail
    export DEBIAN_FRONTEND=noninteractive
    dpkg -i /tmp/veil.deb || apt-get install -f -y -qq
'

echo "==> Starting veil service"
docker exec "${CONTAINER_NAME}" systemctl enable veil
docker exec "${CONTAINER_NAME}" systemctl restart veil

echo "==> Waiting for veil to become active"
for _ in $(seq 1 30); do
    if docker exec "${CONTAINER_NAME}" systemctl is-active veil 2>/dev/null | grep -q '^active$'; then
        break
    fi
    sleep 2
done

if ! docker exec "${CONTAINER_NAME}" systemctl is-active veil | grep -q '^active$'; then
    echo "ERROR: veil service failed to start" >&2
    docker exec "${CONTAINER_NAME}" journalctl -u veil --no-pager -n 80 >&2 || true
    exit 1
fi

echo "==> HTTP redirect check (port 80)"
docker exec "${CONTAINER_NAME}" curl -skI http://127.0.0.1/ | tee /tmp/veil-http-check.txt
grep -qi '301\|302\|location: https' /tmp/veil-http-check.txt

echo "==> HTTPS content check (port 443)"
docker exec "${CONTAINER_NAME}" curl -sk https://127.0.0.1/ | tee /tmp/veil-https-check.txt
grep -qi 'VEIL' /tmp/veil-https-check.txt

echo "==> All checks passed"