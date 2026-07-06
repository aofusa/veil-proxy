#!/usr/bin/env bash
# F-86: OS/Syscall レイヤーのフォールトインジェクション（container_security レビュー提案3）
#
# Toxiproxy/Pumba はネットワーク層の障害注入だが、io_uring 自体のシステムコールレベルの
# エラーハンドリングはテストできていない。strace の syscall フォールトインジェクション
# （`-e inject=io_uring_enter:error=EBUSY` 等）で、カーネルの io_uring_enter(2) に人為的な
# エラー（EBUSY/ENOMEM/EFAULT/EINTR）を注入しつつ HTTP 負荷を与え、Veil が
# 「panic/abort/segfault せず、注入停止後にヘルスが回復する」ことを検証する。
#
# strace の ptrace には CAP_SYS_PTRACE が必要なため、suite 本体コンテナとは分離した専用
# コンテナ（--cap-add SYS_PTRACE、seccomp unconfined）を立てる（既定 SKIP）。docker のみ。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../../.." && pwd)"
RESULTS_DIR="${RESULTS_DIR:-${REPO_ROOT}/tools/container_security/results}"
DOCKER_DIR="${REPO_ROOT}/docker"
NET_NAME="${NET_NAME:-veil-sec-test-net}"
VEIL_IMAGE="${VEIL_IMAGE:-veil:glibc}"
SC_CONTAINER="${SC_CONTAINER:-veil-sec-syscall}"
# 注入対象の syscall とエラーコード（空白区切り。io_uring_enter を主軸に、submit/complete
# 経路に効く EBUSY/ENOMEM/EFAULT/EINTR を順に試す）。
SC_INJECTIONS="${SC_INJECTIONS:-io_uring_enter:error=EBUSY io_uring_enter:error=ENOMEM io_uring_enter:error=EINTR io_uring_setup:error=EFAULT}"
# 注入開始オフセット（when=<first>+<step>）。起動時の ring 初期化 io_uring_enter を避け、
# 稼働中イベントループの io_uring_enter に効かせるため、既定は数十回目以降から注入する。
SC_WHEN_FIRST="${SC_WHEN_FIRST:-40}"
# 注入間隔（<step> 回に 1 回）。1 に近いほど厳しい。既定は 5 回に 1 回。
SC_WHEN_STEP="${SC_WHEN_STEP:-5}"
SC_LOAD_SECONDS="${SC_LOAD_SECONDS:-15}"
REPORT="${RESULTS_DIR}/syscall_chaos_report.txt"

mkdir -p "${RESULTS_DIR}"

if [[ "${SKIP_SYSCALL_CHAOS:-1}" == "1" ]]; then
    echo "syscall_chaos: skipped (SKIP_SYSCALL_CHAOS=1)" | tee "${REPORT}"
    exit 0
fi

cleanup_sc() { docker rm -f "${SC_CONTAINER}" >/dev/null 2>&1 || true; }
trap cleanup_sc EXIT

: >"${REPORT}"
echo "syscall_chaos start when=${SC_WHEN_FIRST}+${SC_WHEN_STEP} load=${SC_LOAD_SECONDS}s" | tee -a "${REPORT}"

docker network inspect "${NET_NAME}" >/dev/null 2>&1 || docker network create "${NET_NAME}" >/dev/null

# strace 入りの実行イメージを用意（Veil バイナリを debian:bookworm-slim + strace 上で動かす）。
# Veil イメージは distroless 相当で strace を含まないため、バイナリを取り出して同梱する。
SC_RUN_IMAGE="veil-sec-strace:local"
build_strace_image() {
    echo "== strace 実行イメージをビルド ==" | tee -a "${REPORT}"
    # ビルドコンテキストは results 配下（リポジトリツリー内）に置く。docker デーモンが
    # ホストの /tmp を参照できない環境（rootless 等）でも build できるようにするため。
    local ctx
    ctx="${RESULTS_DIR}/syscall-strace-ctx"
    rm -rf "${ctx}"
    mkdir -p "${ctx}"
    # Veil バイナリを取り出す（docker cp は環境により無音失敗するため export | tar で抽出）。
    local cid
    cid=$(docker create "${VEIL_IMAGE}")
    if ! docker export "${cid}" | tar -x -C "${ctx}" veil usr/local/bin/veil 2>/dev/null; then
        docker export "${cid}" | tar -x -C "${ctx}" veil 2>/dev/null || true
    fi
    docker rm -f "${cid}" >/dev/null 2>&1 || true
    if [[ ! -f "${ctx}/veil" && -f "${ctx}/usr/local/bin/veil" ]]; then
        mv "${ctx}/usr/local/bin/veil" "${ctx}/veil"
    fi
    if [[ ! -f "${ctx}/veil" ]]; then
        rm -rf "${ctx}"
        echo "syscall_chaos: veil バイナリを取得できずスキップ" | tee -a "${REPORT}"
        exit 0
    fi
    cat >"${ctx}/Dockerfile" <<'DOCKERFILE'
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends strace ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY veil /usr/local/bin/veil
RUN chmod +x /usr/local/bin/veil
ENTRYPOINT ["/usr/local/bin/veil"]
DOCKERFILE
    docker build -q -t "${SC_RUN_IMAGE}" "${ctx}" >>"${REPORT}" 2>&1 || {
        echo "syscall_chaos: strace イメージのビルドに失敗しスキップ" | tee -a "${REPORT}"; exit 0;
    }
    rm -rf "${ctx}"
}
build_strace_image

# 単一の注入設定で 1 回検証する。
# 出力: "<injection> runtime=<ok|panic|exited> recovered=<yes|no|skip>"
run_one_injection() {
    local inject="$1"
    cleanup_sc
    local when="when=${SC_WHEN_FIRST}+${SC_WHEN_STEP}"

    # strace でラップして Veil を起動（io_uring_enter 等に人為エラーを注入）。
    # -f: 子スレッド追跡（thread-per-core）。エラー注入対象以外はそのまま通す。
    if ! docker run -d --name "${SC_CONTAINER}" --network "${NET_NAME}" \
        --cap-add SYS_PTRACE --security-opt seccomp=unconfined \
        --tmpfs /var/cache/veil:rw,nosuid,uid=0,gid=0,size=64m \
        --tmpfs /var/tmp/veil:rw,nosuid,uid=0,gid=0,size=32m \
        -v "${DOCKER_DIR}/assets/conf.d/config.toml:/etc/veil/conf.d/config.toml:ro" \
        -v "${DOCKER_DIR}/assets/ssl:/etc/veil/ssl:ro" \
        -v "${DOCKER_DIR}/assets/www:/var/www:ro" \
        --entrypoint strace \
        "${SC_RUN_IMAGE}" \
        -f -e "inject=${inject}:${when}" /usr/local/bin/veil -c /etc/veil/conf.d/config.toml \
        >/dev/null 2>>"${REPORT}"; then
        echo "${inject} runtime=start-error recovered=skip"
        return
    fi

    local ip
    ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "${SC_CONTAINER}" 2>/dev/null || true)
    sleep 4

    # --- 注入中の負荷（curl 連打。strace 下で低速のため軽め）---
    if [[ -n "${ip}" ]]; then
        docker run --rm --network "${NET_NAME}" curlimages/curl:latest \
            sh -c "for i in \$(seq 1 40); do curl -sk --max-time 3 https://${ip}:443/ >/dev/null 2>&1 || true; done" \
            >>"${REPORT}" 2>&1 || true
    fi

    # panic / abort / segfault の検査。exit code 0 の終了は graceful（fault で進行不能なら
    # 安全にシャットダウンするのが正しい挙動。panic/segfault/hang でないことが合格条件）。
    local runtime status exitcode
    status=$(docker inspect -f '{{.State.Status}}' "${SC_CONTAINER}" 2>/dev/null || echo absent)
    exitcode=$(docker inspect -f '{{.State.ExitCode}}' "${SC_CONTAINER}" 2>/dev/null || echo "?")
    if docker logs "${SC_CONTAINER}" 2>&1 | grep -qiE 'panic|RUST_BACKTRACE|segfault|SIGSEGV|SIGABRT'; then
        runtime="panic"
    elif [[ "${status}" == "running" ]]; then
        runtime="ok"
    elif [[ "${exitcode}" == "0" ]]; then
        runtime="graceful-exit"
    else
        runtime="exited(${exitcode})"
    fi

    # --- 回復フェーズ: 注入は when=1+step のため一定確率で通し続けるが、負荷停止後に
    #     プロセスが生存していればヘルスが応答するはず。数回リトライで判定する。---
    local recovered="no"
    if [[ "${runtime}" == "ok" && -n "${ip}" ]]; then
        local r
        for r in 1 2 3 4 5; do
            if docker run --rm --network "${NET_NAME}" curlimages/curl:latest \
                curl -sk --max-time 5 -o /dev/null -w '%{http_code}' "https://${ip}:443/" 2>/dev/null \
                | grep -qE '^[0-9]{3}$'; then
                recovered="yes"; break
            fi
            sleep 1
        done
    else
        recovered="skip"
    fi

    if [[ "${runtime}" == graceful-exit || "${runtime}" == exited* ]]; then
        {
            echo "  --- ${inject} container exited; last logs ---"
            docker logs --tail 8 "${SC_CONTAINER}" 2>&1 | sed 's/^/  | /'
        } >>"${REPORT}"
    fi
    echo "${inject} runtime=${runtime} recovered=${recovered}"
}

echo "== io_uring syscall フォールトインジェクション ==" | tee -a "${REPORT}"
fail=0
for inject in ${SC_INJECTIONS}; do
    line="$(run_one_injection "${inject}")"
    echo "  ${line}" | tee -a "${REPORT}"
    # panic は安全性違反 = findings。exited（注入エラーで graceful に終了）は許容だが記録。
    if echo "${line}" | grep -qE 'runtime=panic'; then
        echo "  WARNING: ${inject} で panic を検出（backlog 起票対象）" | tee -a "${REPORT}"
        docker logs "${SC_CONTAINER}" 2>&1 | grep -iE 'panic|abort|segfault' | head -5 | tee -a "${REPORT}" || true
        fail=1
    fi
done

echo "syscall_chaos: done (findings=${fail}; SQ 満杯ハングは B-24 で修正済み)" | tee -a "${REPORT}"
if [[ "${SYSCALL_CHAOS_BLOCKING:-0}" == "1" && "${fail}" != "0" ]]; then
    exit 1
fi
