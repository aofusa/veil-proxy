#!/usr/bin/env bash
# container_security テスト共通ヘルパ（docker コマンドのみ使用）
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
DOCKER_DIR="${REPO_ROOT}/docker"

# テスト用リソース名（衝突回避のためプレフィックス付き）
export NET_NAME="${NET_NAME:-veil-sec-test-net}"
export VEIL_CONTAINER="${VEIL_CONTAINER:-veil-sec-proxy}"
export HARNESS_CONTAINER="${HARNESS_CONTAINER:-veil-sec-harness}"
export TRIVY_CONTAINER="${TRIVY_CONTAINER:-veil-sec-trivy}"
export VEIL_IMAGE="${VEIL_IMAGE:-veil:glibc}"
export HARNESS_IMAGE="${HARNESS_IMAGE:-veil-sec-harness:local}"
export RESULTS_DIR="${RESULTS_DIR:-${REPO_ROOT}/tests/container_security/results}"

log() {
    printf '[container_security] %s\n' "$*" >&2
}

die() {
    log "ERROR: $*"
    exit 1
}

require_docker() {
    command -v docker >/dev/null 2>&1 || die "docker が見つかりません"
    docker info >/dev/null 2>&1 || die "docker daemon に接続できません"
}

cleanup() {
    log "クリーンアップ中..."
    docker rm -f "${HARNESS_CONTAINER}" "${TRIVY_CONTAINER}" "${VEIL_CONTAINER}" 2>/dev/null || true
    docker network rm "${NET_NAME}" 2>/dev/null || true
}

wait_veil_healthy() {
    local attempts="${1:-60}"
    local i
    for ((i = 1; i <= attempts; i++)); do
        if docker run --rm \
            --network "${NET_NAME}" \
            "${HARNESS_IMAGE}" \
            health >/dev/null 2>&1; then
            log "Veil が応答可能 (${i}/${attempts})"
            return 0
        fi
        sleep 1
    done
    die "Veil が起動しませんでした"
}

start_network() {
    if ! docker network inspect "${NET_NAME}" >/dev/null 2>&1; then
        docker network create "${NET_NAME}" >/dev/null
        log "ネットワーク作成: ${NET_NAME}"
    fi
}

build_harness_image() {
    log "ハーネスイメージをビルド: ${HARNESS_IMAGE}"
    docker build \
        -t "${HARNESS_IMAGE}" \
        -f "${REPO_ROOT}/tests/container_security/harness/Dockerfile" \
        "${REPO_ROOT}/tests/container_security/harness"
}

start_veil_container() {
    local seccomp_path="${DOCKER_DIR}/assets/security/seccomp.json"
    [[ -f "${seccomp_path}" ]] || die "seccomp プロファイルが見つかりません: ${seccomp_path}"
    [[ -f "${DOCKER_DIR}/assets/ssl/cert.pem" ]] || die "TLS 証明書が見つかりません。docker/README.md を参照して生成してください"
    local test_config="${REPO_ROOT}/tests/container_security/fixtures/veil-config.toml"
    [[ -f "${test_config}" ]] || die "テスト用設定が見つかりません: ${test_config}"

    log "Veil コンテナ起動: ${VEIL_CONTAINER} (${VEIL_IMAGE})"
    docker run -d \
        --name "${VEIL_CONTAINER}" \
        --network "${NET_NAME}" \
        --hostname veil-proxy \
        --read-only \
        --tmpfs /var/cache/veil:rw,noexec,nosuid,uid=65532,gid=65532,size=512m \
        --tmpfs /var/tmp/veil:rw,noexec,nosuid,uid=65532,gid=65532,size=256m \
        -v "${test_config}:/etc/veil/conf.d/config.toml:ro" \
        -v "${DOCKER_DIR}/assets/ssl:/etc/veil/ssl:ro" \
        -v "${DOCKER_DIR}/assets/www:/var/www:ro" \
        --security-opt "seccomp=${seccomp_path}" \
        "${VEIL_IMAGE}" >/dev/null
}

run_harness() {
    local phase="$1"
    shift
    log "ハーネス実行: ${phase}"
    mkdir -p "${RESULTS_DIR}"
    docker run --rm \
        --name "${HARNESS_CONTAINER}-${phase}" \
        --network "${NET_NAME}" \
        -e "VEIL_HOST=${VEIL_CONTAINER}" \
        -e "VEIL_HTTP_PORT=80" \
        -e "VEIL_HTTPS_PORT=443" \
        -e "VEIL_H2C_PORT=8443" \
        -e "H2SPEC_FULL=${H2SPEC_FULL:-0}" \
        -e "H2SPEC_STRICT=${H2SPEC_STRICT:-0}" \
        -e "H2SPEC_TIMEOUT=${H2SPEC_TIMEOUT:-30}" \
        -e "PHASE=${phase}" \
        -v "${RESULTS_DIR}:/results:rw" \
        "${HARNESS_IMAGE}" \
        "$@"
}

inspect_container_security() {
    local out="${RESULTS_DIR}/container_inspect.json"
    docker inspect "${VEIL_CONTAINER}" >"${out}"
    log "コンテナ inspect 結果: ${out}"
}