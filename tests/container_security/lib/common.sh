#!/usr/bin/env bash
# container_security テスト共通ヘルパ（docker コマンドのみ使用）
set -euo pipefail

LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${LIB_DIR}/../../.." && pwd)"
DOCKER_DIR="${REPO_ROOT}/docker"
# shellcheck source=capabilities.sh
source "${LIB_DIR}/capabilities.sh"

# テスト用リソース名（衝突回避のためプレフィックス付き）
export NET_NAME="${NET_NAME:-veil-sec-test-net}"
export VEIL_CONTAINER="${VEIL_CONTAINER:-veil-sec-proxy}"
export HARNESS_CONTAINER="${HARNESS_CONTAINER:-veil-sec-harness}"
export TRIVY_CONTAINER="${TRIVY_CONTAINER:-veil-sec-trivy}"
export VEIL_IMAGE="${VEIL_IMAGE:-veil:glibc}"
export HARNESS_IMAGE="${HARNESS_IMAGE:-veil-sec-harness:local}"
export RESULTS_DIR="${RESULTS_DIR:-${REPO_ROOT}/tests/container_security/results}"
export BACKEND_CONTAINER="${BACKEND_CONTAINER:-veil-sec-backend}"
export TOXIPROXY_CONTAINER="${TOXIPROXY_CONTAINER:-veil-sec-toxiproxy}"
export CHAOS_TIMEOUT_SEC="${CHAOS_TIMEOUT_SEC:-300}"
export RELOAD_POLL_ATTEMPTS="${RELOAD_POLL_ATTEMPTS:-30}"

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
    docker rm -f "${HARNESS_CONTAINER}" "${TRIVY_CONTAINER}" "${VEIL_CONTAINER}" \
        "${BACKEND_CONTAINER}" "${TOXIPROXY_CONTAINER}" 2>/dev/null || true
    docker network rm "${NET_NAME}" 2>/dev/null || true
}

wait_with_timeout() {
    local pid="$1"
    local timeout_sec="$2"
    local label="${3:-process}"
    local elapsed=0
    while kill -0 "${pid}" 2>/dev/null; do
        if [[ "${elapsed}" -ge "${timeout_sec}" ]]; then
            log "ERROR: ${label} が ${timeout_sec}s でタイムアウト — 強制終了"
            kill -TERM "${pid}" 2>/dev/null || true
            sleep 2
            kill -KILL "${pid}" 2>/dev/null || true
            return 1
        fi
        sleep 1
        elapsed=$((elapsed + 1))
    done
    wait "${pid}"
}

resolve_veil_host() {
    docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "${VEIL_CONTAINER}" 2>/dev/null
}

wait_veil_healthy() {
    local attempts="${1:-60}"
    local veil_host
    veil_host="$(resolve_veil_host)"
    [[ -n "${veil_host}" ]] || die "Veil コンテナ IP を取得できません"
    local i
    for ((i = 1; i <= attempts; i++)); do
        if docker run --rm \
            --network "${NET_NAME}" \
            -e "VEIL_HOST=${veil_host}" \
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

prepare_veil_test_config() {
    local src="${REPO_ROOT}/tests/container_security/fixtures/veil-config.toml"
    local dst="${RESULTS_DIR}/veil-config.runtime.toml"
    [[ -f "${src}" ]] || die "テスト用設定が見つかりません: ${src}"
    mkdir -p "${RESULTS_DIR}"
    cp "${src}" "${dst}"

    # Landlock 下では glibc の NSS が /usr 配下を参照する。Docker DNS ホスト名は
    # 起動時に IP へ置換して上流接続の安定性を確保する（F-53 Toxiproxy 経路）。
    if docker inspect "${TOXIPROXY_CONTAINER}" >/dev/null 2>&1; then
        local toxi_ip
        toxi_ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' \
            "${TOXIPROXY_CONTAINER}" 2>/dev/null || true)
        if [[ -n "${toxi_ip}" ]]; then
            sed -i "s|http://veil-sec-toxiproxy:8480/|http://${toxi_ip}:8480/|g" "${dst}"
            log "Toxiproxy 上流を IP 指定に設定: ${toxi_ip}:8480"
        fi
    fi
    echo "${dst}"
}

start_veil_container() {
    local seccomp_path="${DOCKER_DIR}/assets/security/seccomp.json"
    [[ -f "${seccomp_path}" ]] || die "seccomp プロファイルが見つかりません: ${seccomp_path}"
    [[ -f "${DOCKER_DIR}/assets/ssl/cert.pem" ]] || die "TLS 証明書が見つかりません。docker/README.md を参照して生成してください"
    local test_config
    test_config="$(prepare_veil_test_config)"

    log "Veil コンテナ起動: ${VEIL_CONTAINER} (${VEIL_IMAGE})"
    docker run -d \
        --name "${VEIL_CONTAINER}" \
        --network "${NET_NAME}" \
        --network-alias veil-proxy \
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
    local veil_host
    veil_host="$(resolve_veil_host)"
    [[ -n "${veil_host}" ]] || die "Veil コンテナ IP を取得できません (${phase})"
    log "ハーネス実行: ${phase} (veil=${veil_host})"
    mkdir -p "${RESULTS_DIR}"
    docker run --rm \
        --name "${HARNESS_CONTAINER}-${phase}" \
        --network "${NET_NAME}" \
        -e "VEIL_HOST=${veil_host}" \
        -e "VEIL_HTTP_PORT=80" \
        -e "VEIL_HTTPS_PORT=443" \
        -e "VEIL_H2C_PORT=8443" \
        -e "H2SPEC_FULL=${H2SPEC_FULL:-0}" \
        -e "H2SPEC_STRICT=${H2SPEC_STRICT:-0}" \
        -e "H2SPEC_TIMEOUT=${H2SPEC_TIMEOUT:-30}" \
        -e "TOXIPROXY_HOST=${TOXIPROXY_CONTAINER}" \
        -e "SKIP_TESTSSL=${SKIP_TESTSSL:-0}" \
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