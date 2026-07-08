#!/usr/bin/env bash
# 不正 WASM モジュールの起動失敗検証（W-04）。ホスト側 docker から ephemeral Veil を起動。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../../.." && pwd)"
RESULTS="${RESULTS_DIR:-${REPO_ROOT}/tools/container_security/results}/wasm_invalid_module_report.txt"

VEIL_IMAGE="${VEIL_IMAGE:-veil:glibc}"
NET_NAME="${NET_NAME:-veil-sec-test-net}"
DOCKER_DIR="${REPO_ROOT}/docker"
VEIL_CONTAINER="${VEIL_CONTAINER:-veil-sec-proxy}"

mkdir -p "$(dirname "${RESULTS}")"
: >"${RESULTS}"

log() { printf '%s\n' "$*" | tee -a "${RESULTS}"; }

fails=0

log "wasm_invalid_module_probe start"

base_config="${REPO_ROOT}/tools/container_security/fixtures/veil-config.toml"
invalid_config="${RESULTS_DIR:-${REPO_ROOT}/tools/container_security/results}/veil-config.invalid-wasm.toml"
seccomp_path="${DOCKER_DIR}/assets/security/seccomp.json"
wasm_dir="${REPO_ROOT}/tests/fixtures/wasm"

[[ -f "${base_config}" ]] || { log "FAIL missing base config"; exit 1; }
[[ -f "${seccomp_path}" ]] || { log "FAIL missing seccomp"; exit 1; }
[[ -f "${wasm_dir}/invalid.wasm" ]] || { log "FAIL missing invalid.wasm fixture"; exit 1; }

cp "${base_config}" "${invalid_config}"
cat >>"${invalid_config}" <<'EOF'

[[wasm.modules]]
name = "corrupt_filter"
path = "/etc/veil/wasm/invalid.wasm"
configuration = '{}'

  [wasm.modules.capabilities]
  allow_logging = true
  allow_response_headers_write = true
EOF

ephemeral="veil-sec-invalid-wasm-$$"
set +e
docker rm -f "${ephemeral}" >/dev/null 2>&1
docker run -d \
    --name "${ephemeral}" \
    --network "${NET_NAME}" \
    --read-only \
    --tmpfs /var/cache/veil:rw,noexec,nosuid,uid=65532,gid=65532,size=128m \
    --tmpfs /var/tmp/veil:rw,noexec,nosuid,uid=65532,gid=65532,size=64m \
    -v "${invalid_config}:/etc/veil/conf.d/config.toml:ro" \
    -v "${DOCKER_DIR}/assets/ssl:/etc/veil/ssl:ro" \
    -v "${DOCKER_DIR}/assets/www:/var/www:ro" \
    -v "${wasm_dir}:/etc/veil/wasm:ro" \
    --security-opt "seccomp=${seccomp_path}" \
    "${VEIL_IMAGE}" >/dev/null 2>&1
rc_run=$?
set -e

if [[ "${rc_run}" -ne 0 ]]; then
    log "PASS invalid_wasm_startup_rejected (docker run failed immediately)"
else
    # 起動後すぐ終了またはログにロード失敗が出ることを期待
    sleep 3
    running=$(docker inspect -f '{{.State.Running}}' "${ephemeral}" 2>/dev/null || echo "false")
    logs=$(docker logs "${ephemeral}" 2>&1 | tail -50 || true)
    docker rm -f "${ephemeral}" >/dev/null 2>&1 || true

    if [[ "${running}" != "true" ]]; then
        log "PASS invalid_wasm_container_exited"
    elif echo "${logs}" | grep -qiE 'WASM|wasm|Module|error|failed'; then
        log "PASS invalid_wasm_load_error_logged (container may still be running)"
    else
        log "FAIL invalid_wasm_expected_startup_failure running=${running}"
        fails=$((fails + 1))
    fi
fi

# メイン Veil が生存していること
ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' \
    "${VEIL_CONTAINER}" 2>/dev/null || true)
if [[ -n "${ip}" ]]; then
    hc=$(curl -sk -o /dev/null -w "%{http_code}" --max-time 5 \
        "https://${ip}:443/" 2>/dev/null || echo "000")
    if [[ "${hc}" == "200" ]]; then
        log "main_veil_health: ok"
    else
        log "main_veil_health: fail (${hc})"
        fails=$((fails + 1))
    fi
else
    log "WARN main_veil_health: skipped (no ip)"
fi

if [[ "${fails}" -eq 0 ]]; then
    log "wasm_invalid_module: ok"
    exit 0
fi
log "wasm_invalid_module: FAILURES (fails=${fails})"
exit 1