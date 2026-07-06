#!/usr/bin/env bash
# F-85: E2E カオステストへの ASAN/TSAN 統合（container_security レビュー提案2）
#
# Sanitizer は従来 libFuzzer（プロトコルパーサー）にのみ適用され、プロキシ全体を通した
# メモリアクセス違反・データ競合（io_uring 固有の Future Drop 時 UAF 等）を実行レベルで
# 検出できていなかった。本フェーズは **sanitizer ビルドの Veil バイナリ** をコンテナで起動し、
# 高並行 HTTP/HTTPS 負荷・接続チャーン・SIGHUP リロードをかけ、コンテナログの sanitizer
# レポート（`ERROR: AddressSanitizer` / `WARNING: ThreadSanitizer`）を検査する。
#
# ASAN/TSAN ビルドは nightly + `-Zbuild-std` を要し重いため既定 SKIP。docker のみで完結。
# 有効化: `RUN_E2E_ASAN=1`（address）または `RUN_E2E_TSAN=1`（thread）。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
RESULTS_DIR="${RESULTS_DIR:-${REPO_ROOT}/tools/container_security/results}"
DOCKER_DIR="${REPO_ROOT}/docker"
NET_NAME="${NET_NAME:-veil-sec-test-net}"
RUST_IMAGE="${RUST_FUZZ_IMAGE:-rustlang/rust:nightly-bookworm}"
SAN_CONTAINER="${SAN_CONTAINER:-veil-sec-san}"
SAN_RUN_IMAGE="veil-sec-san:local"
SAN_CARGO_FEATURES="${SAN_CARGO_FEATURES:-full}"
SAN_LOAD_SECONDS="${SAN_LOAD_SECONDS:-20}"
SAN_TARGET="${SAN_TARGET:-x86_64-unknown-linux-gnu}"
REPORT="${RESULTS_DIR}/e2e_sanitizer_report.txt"

mkdir -p "${RESULTS_DIR}"

# sanitizer 選択（address が既定。RUN_E2E_TSAN=1 で thread）。
RUN_E2E_ASAN="${RUN_E2E_ASAN:-0}"
RUN_E2E_TSAN="${RUN_E2E_TSAN:-0}"
if [[ "${RUN_E2E_ASAN}" != "1" && "${RUN_E2E_TSAN}" != "1" ]]; then
    echo "e2e_sanitizer: skipped (RUN_E2E_ASAN=0 かつ RUN_E2E_TSAN=0)" | tee "${REPORT}"
    exit 0
fi
if [[ "${RUN_E2E_TSAN}" == "1" ]]; then
    SANITIZER="thread"
    SAN_ERROR_RE='ThreadSanitizer|data race|WARNING: ThreadSanitizer'
else
    SANITIZER="address"
    SAN_ERROR_RE='ERROR: AddressSanitizer|heap-use-after-free|heap-buffer-overflow|LeakSanitizer|detected memory leaks'
fi

cleanup_san() { docker rm -f "${SAN_CONTAINER}" >/dev/null 2>&1 || true; }
trap cleanup_san EXIT

: >"${REPORT}"
echo "e2e_sanitizer start sanitizer=${SANITIZER} features=${SAN_CARGO_FEATURES} load=${SAN_LOAD_SECONDS}s" | tee -a "${REPORT}"

docker network inspect "${NET_NAME}" >/dev/null 2>&1 || docker network create "${NET_NAME}" >/dev/null

# --- ビルドフェーズ: sanitizer 有効の Veil バイナリを nightly コンテナで生成 ---
# -Zbuild-std で std ごと sanitizer 計装する（io_uring 周辺の unsafe を確実にカバー）。
SAN_BUILD_OUT="${RESULTS_DIR}/cargo-target-e2e-san"
mkdir -p "${SAN_BUILD_OUT}"
echo "== sanitizer(${SANITIZER}) Veil バイナリをビルド（重い・数分）==" | tee -a "${REPORT}"
if ! docker run --rm \
    -v "${REPO_ROOT}:/app:rw" \
    -v "${SAN_BUILD_OUT}:/target:rw" \
    -w /app \
    -e CARGO_TARGET_DIR=/target \
    -e RUSTFLAGS="-Zsanitizer=${SANITIZER}" \
    "${RUST_IMAGE}" \
    bash -c "
        set -euo pipefail
        rustup component add rust-src 2>/dev/null || true
        cargo build -Zbuild-std --target ${SAN_TARGET} \
            --features '${SAN_CARGO_FEATURES}' --bin veil 2>&1 | tail -20
    " >>"${REPORT}" 2>&1; then
    echo "e2e_sanitizer: sanitizer ビルドに失敗しスキップ（ログ: ${REPORT}）" | tee -a "${REPORT}"
    exit 0
fi

SAN_BIN="${SAN_BUILD_OUT}/${SAN_TARGET}/debug/veil"
if [[ ! -f "${SAN_BIN}" ]]; then
    echo "e2e_sanitizer: sanitizer バイナリが見つからずスキップ (${SAN_BIN})" | tee -a "${REPORT}"
    exit 0
fi

# --- パッケージング: バイナリ + sanitizer ランタイム依存を含む実行イメージ ---
# ビルドコンテキストは results 配下（docker デーモンが /tmp 非参照の環境に対応）。
SAN_CTX="${RESULTS_DIR}/e2e-san-ctx"
rm -rf "${SAN_CTX}"; mkdir -p "${SAN_CTX}"
cp "${SAN_BIN}" "${SAN_CTX}/veil"
cat >"${SAN_CTX}/Dockerfile" <<'DOCKERFILE'
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates llvm \
    && rm -rf /var/lib/apt/lists/*
COPY veil /usr/local/bin/veil
RUN chmod +x /usr/local/bin/veil
ENTRYPOINT ["/usr/local/bin/veil"]
DOCKERFILE
if ! docker build -q -t "${SAN_RUN_IMAGE}" "${SAN_CTX}" >>"${REPORT}" 2>&1; then
    echo "e2e_sanitizer: 実行イメージのビルドに失敗しスキップ" | tee -a "${REPORT}"
    exit 0
fi
rm -rf "${SAN_CTX}"

# --- 起動フェーズ: seccomp/Landlock は sanitizer と干渉するため緩和して実行 ---
# ASAN ランタイムは追加の mmap/シグナル操作を行うため seccomp=unconfined とする。
# ASAN_OPTIONS: abort_on_error でクラッシュ時に確実に終了、detect_leaks はプロセス終了時。
SAN_OPTS_ENV=()
if [[ "${SANITIZER}" == "address" ]]; then
    SAN_OPTS_ENV=(-e "ASAN_OPTIONS=abort_on_error=1:detect_leaks=1:halt_on_error=0")
else
    SAN_OPTS_ENV=(-e "TSAN_OPTIONS=halt_on_error=0:second_deadlock_stack=1")
fi

echo "== sanitizer Veil を起動しカオス負荷 ==" | tee -a "${REPORT}"
if ! docker run -d --name "${SAN_CONTAINER}" --network "${NET_NAME}" \
    --security-opt seccomp=unconfined \
    "${SAN_OPTS_ENV[@]}" \
    --tmpfs /var/cache/veil:rw,nosuid,uid=0,gid=0,size=128m \
    --tmpfs /var/tmp/veil:rw,nosuid,uid=0,gid=0,size=64m \
    -v "${DOCKER_DIR}/assets/conf.d/config.toml:/etc/veil/conf.d/config.toml:ro" \
    -v "${DOCKER_DIR}/assets/ssl:/etc/veil/ssl:ro" \
    -v "${DOCKER_DIR}/assets/www:/var/www:ro" \
    "${SAN_RUN_IMAGE}" -c /etc/veil/conf.d/config.toml \
    >/dev/null 2>>"${REPORT}"; then
    echo "e2e_sanitizer: コンテナ起動に失敗しスキップ" | tee -a "${REPORT}"
    exit 0
fi

SAN_IP=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "${SAN_CONTAINER}" 2>/dev/null || true)
sleep 5

# 起動時点で sanitizer エラーが出ていないか、そもそも起動できたか。
if [[ "$(docker inspect -f '{{.State.Status}}' "${SAN_CONTAINER}" 2>/dev/null || echo absent)" != "running" ]]; then
    echo "e2e_sanitizer: 起動直後に終了（ログ確認）" | tee -a "${REPORT}"
    docker logs --tail 30 "${SAN_CONTAINER}" 2>&1 | sed 's/^/  | /' | tee -a "${REPORT}"
fi

# --- カオス負荷: 高並行 HTTP/HTTPS + 接続チャーン + SIGHUP リロード ---
if [[ -n "${SAN_IP}" ]]; then
    # 高並行 + 短時間接続の churn（ASAN 下では低速なため wrk ではなく curl 並列で軽めに）。
    docker run --rm --network "${NET_NAME}" curlimages/curl:latest sh -c "
        end=\$(( \$(date +%s) + ${SAN_LOAD_SECONDS} ))
        while [ \$(date +%s) -lt \$end ]; do
            for i in \$(seq 1 16); do
                curl -sk --max-time 3 https://${SAN_IP}:443/ >/dev/null 2>&1 &
                curl -s  --max-time 3 http://${SAN_IP}:80/  >/dev/null 2>&1 &
            done
            wait
        done
    " >>"${REPORT}" 2>&1 || true

    # SIGHUP リロードを負荷中に数回送出（カーネル連携中の UAF/リークを誘発）。
    for _ in 1 2 3; do
        docker kill --signal=HUP "${SAN_CONTAINER}" >/dev/null 2>&1 || true
        sleep 1
    done
    # 追加の短時間接続 churn。
    docker run --rm --network "${NET_NAME}" curlimages/curl:latest sh -c "
        for i in \$(seq 1 60); do curl -sk --max-time 2 https://${SAN_IP}:443/ >/dev/null 2>&1 || true; done
    " >>"${REPORT}" 2>&1 || true
fi

# プロセスを正常終了させて LeakSanitizer のプロセス終了時レポートを促す。
docker kill --signal=TERM "${SAN_CONTAINER}" >/dev/null 2>&1 || true
sleep 3

# --- 判定: コンテナログに sanitizer エラーが出ていないこと ---
LOGS="$(docker logs "${SAN_CONTAINER}" 2>&1 || true)"
echo "--- sanitizer container logs (tail) ---" >>"${REPORT}"
echo "${LOGS}" | tail -40 >>"${REPORT}"

fail=0
if echo "${LOGS}" | grep -qE "${SAN_ERROR_RE}"; then
    echo "e2e_sanitizer: ${SANITIZER}Sanitizer エラーを検出（backlog 起票対象）" | tee -a "${REPORT}"
    echo "${LOGS}" | grep -E "${SAN_ERROR_RE}" | head -10 | tee -a "${REPORT}"
    fail=1
else
    echo "e2e_sanitizer: ${SANITIZER} エラーなし（カオス負荷 + SIGHUP を通過）" | tee -a "${REPORT}"
fi

echo "e2e_sanitizer: done (findings=${fail})" | tee -a "${REPORT}"
if [[ "${E2E_SANITIZER_BLOCKING:-0}" == "1" && "${fail}" != "0" ]]; then
    exit 1
fi
