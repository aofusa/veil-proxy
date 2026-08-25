#!/usr/bin/env bash
# HTTP/3（QUIC）構成の交互 A/B ラボハーネス（F-161 で追加）
#
# `h2c_proxy_lab.sh` の HTTP/3 版。`run_perf.sh` は 1 構成の計測が終わるとコンテナを
# 落としてしまい、また 1 イメージぶんしか測れないため「改修前後の 2 イメージを
# 交互に測る」ことができない。本スクリプトは HTTP/3 構成（既定 `h3_proxy`）だけを
# 対象に、`h2c_proxy_lab.sh ab` と同じ手順（毎ラウンド持続負荷で定常状態にしてから
# 計測、ラウンドごとに実行順を入替）で交互 A/B を取る。
#
# 使い方:
#   bash tools/perf/h3_ab.sh up  [config] [image]              # 上流 + veil を起動
#   bash tools/perf/h3_ab.sh load [h2load 引数...]             # 1 回計測して rps を出す
#   bash tools/perf/h3_ab.sh ab <base_img> <new_img> [config] [rounds]
#   bash tools/perf/h3_ab.sh down                              # 後始末
#
# **HTTP/3 の計測には QUIC 対応 h2load（`local/h2load-h3:latest`）が必要**
# （`docker build -t local/h2load-h3:latest tools/perf/h2load-http3`）。
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
ASSETS="$REPO_ROOT/docker/assets"
LOGDIR="$SCRIPT_DIR/results/h3ab"
NET=veil-perf-h3ab
H3_IMG=local/h2load-h3:latest
VEIL_NAME=h3ab-veil
BACKEND_NAME=h3ab-backend
# QUIC は UDP のため、コンテナ間通信で 443/udp をそのまま使う（ポート公開は不要）。
H3_ARGS="${H3_ARGS:---alpn-list=h3}"

mkdir -p "$LOGDIR"

cmd_up() {
    local cfg_name="${1:-h3_proxy}" img="${2:-veil:glibc}"
    local cfgfile="$SCRIPT_DIR/configs/${cfg_name}.toml"
    [ -f "$cfgfile" ] || { echo "!! config が無い: $cfgfile" >&2; return 1; }

    docker network create "$NET" >/dev/null 2>&1 || true
    docker rm -f "$VEIL_NAME" "$BACKEND_NAME" >/dev/null 2>&1

    docker run -d --rm --network "$NET" --network-alias perf-backend \
        -v "$SCRIPT_DIR/nginx/nginx-backend.conf:/etc/nginx/nginx.conf:ro" \
        -v "$ASSETS/www:/var/www:ro" \
        --name "$BACKEND_NAME" nginx:alpine >/dev/null || return 1

    # run_perf.sh と同様に上流ホスト名を実 IP へ置換する（Landlock 下の NSS 回避）。
    local mount_cfg="$LOGDIR/${cfg_name}.runtime.toml"
    cp "$cfgfile" "$mount_cfg"
    local ip
    ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$BACKEND_NAME" 2>/dev/null || true)
    [ -n "$ip" ] && sed -i "s/perf-backend:/${ip}:/g" "$mount_cfg"

    docker run -d --rm --network "$NET" \
        --read-only \
        --tmpfs /var/cache/veil:rw,noexec,nosuid,uid=65532,gid=65532,size=512m \
        --tmpfs /var/tmp/veil:rw,noexec,nosuid,uid=65532,gid=65532,size=256m \
        -v "$mount_cfg:/etc/veil/conf.d/config.toml:ro" \
        -v "$ASSETS/ssl:/etc/veil/ssl:ro" \
        -v "$ASSETS/www:/var/www:ro" \
        -v "$ASSETS/wasm:/etc/veil/wasm:ro" \
        --security-opt seccomp="$ASSETS/security/seccomp.json" \
        --name "$VEIL_NAME" "$img" >/dev/null || return 1

    local i
    for i in $(seq 1 40); do
        if docker run --rm --network "$NET" curlimages/curl:latest -sk -o /dev/null \
             -w '%{http_code}' "https://$VEIL_NAME:443/" 2>/dev/null | grep -q 200; then
            echo "ready: $cfg_name ($img)"
            return 0
        fi
        sleep 0.5
    done
    echo "!! veil not ready" >&2
    docker logs "$VEIL_NAME" 2>&1 | tail -20 >&2
    return 1
}

cmd_load() {
    local args=("$@")
    [ ${#args[@]} -eq 0 ] && args=(-n 30000 -c 100 -m 10)
    docker run --rm --network "$NET" --entrypoint h2load "$H3_IMG" \
        $H3_ARGS "${args[@]}" "https://$VEIL_NAME:443/" 2>&1
}

# 2 イメージの交互 A/B（改修前 vs 改修後）。手順は h2c_proxy_lab.sh の cmd_ab と同じ。
cmd_ab() {
    local base_img="$1" new_img="$2" cfg="${3:-h3_proxy}" rounds="${4:-6}"
    local warm="${WARM_SECS:-40}" nreq="${AB_NREQ:-30000}"

    if ! docker image inspect "$H3_IMG" >/dev/null 2>&1; then
        echo "!! $H3_IMG が無い（tools/perf/h2load-http3 でビルドすること）" >&2
        return 1
    fi
    if [ "$(docker image inspect -f '{{.Id}}' "$base_img" 2>/dev/null)" = \
         "$(docker image inspect -f '{{.Id}}' "$new_img" 2>/dev/null)" ]; then
        echo "!! base と new が同一イメージ。A/B にならない" >&2
        return 1
    fi

    echo -e "round\tvariant\treq_per_sec"
    local r order img variant rps
    for r in $(seq 1 "$rounds"); do
        if [ $((r % 2)) -eq 1 ]; then order="base new"; else order="new base"; fi
        for variant in $order; do
            case "$variant" in
                base) img="$base_img" ;;
                new)  img="$new_img" ;;
            esac
            cmd_up "$cfg" "$img" >/dev/null 2>&1 || { echo -e "$r\t$variant\tNA"; continue; }
            docker run --rm --network "$NET" --entrypoint h2load "$H3_IMG" \
                $H3_ARGS -c 100 -m 10 -D "$warm" "https://$VEIL_NAME:443/" >/dev/null 2>&1
            rps=$(docker run --rm --network "$NET" --entrypoint h2load "$H3_IMG" \
                    $H3_ARGS -n "$nreq" -c 100 -m 10 "https://$VEIL_NAME:443/" 2>&1 \
                  | awk '/finished in/{print $4}')
            echo -e "$r\t$variant\t${rps:-NA}"
        done
    done
}

cmd_down() {
    docker rm -f "$VEIL_NAME" "$BACKEND_NAME" >/dev/null 2>&1
    docker network rm "$NET" >/dev/null 2>&1
    echo "cleaned"
}

case "${1:-}" in
    up)   shift; cmd_up "$@" ;;
    load) shift; cmd_load "$@" ;;
    ab)   shift; cmd_ab "$@" ;;
    down) cmd_down ;;
    *) sed -n '2,18p' "${BASH_SOURCE[0]}"; exit 1 ;;
esac
