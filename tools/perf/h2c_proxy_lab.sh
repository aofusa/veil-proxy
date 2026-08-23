#!/usr/bin/env bash
# h2c_proxy 調査用ラボハーネス（Linux h2c proxy 性能改善）
#
# `run_perf.sh` は 1 構成の計測が終わるとコンテナを落としてしまうため、
# 「負荷をかけたまま CPU 内訳を取る」「strace / perf をアタッチする」といった
# ボトルネック調査ができない。本スクリプトは h2c_proxy（または h2c_file）の
# 構成だけを起動しっぱなしにして、任意の調査コマンドを差し込めるようにする。
#
# 使い方:
#   bash tools/perf/h2c_proxy_lab.sh up   [config] [image]   # 上流 + veil を起動
#   bash tools/perf/h2c_proxy_lab.sh load [path] [h2load 引数...]  # 1 回計測して rps を出す
#   bash tools/perf/h2c_proxy_lab.sh cpu  [path] [秒]        # 負荷中の CPU 内訳を取る
#   bash tools/perf/h2c_proxy_lab.sh strace [path] [件数]    # 1 req あたりの syscall 回数
#   bash tools/perf/h2c_proxy_lab.sh mem  [path] [秒]        # 持続負荷中のメモリ推移
#   bash tools/perf/h2c_proxy_lab.sh down                    # 後始末
#
# `path` は `/`（= 54,576B の index.html）または `/3b.html`（= 3B）。
# **バイト単価と固定費を切り分けるために両方を測ること**（AGENTS.md / F-158 の教訓:
# 「比がサイズで変わらないならコピーではなく固定費が支配項」）。
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
ASSETS="$REPO_ROOT/docker/assets"
LOGDIR="$SCRIPT_DIR/results/lab"
NET=veil-perf-lab
H2_IMG=local/h2load:latest
VEIL_NAME=lab-veil
NGINX_NAME=lab-nginx
BACKEND_NAME=lab-backend
H2C_PORT=8080
# 負荷の宛先コンテナ。既定は veil。nginx ベースラインを測るときは
# `TARGET=lab-nginx` を指定する（`upnginx` で起動しておくこと）。
TARGET="${TARGET:-$VEIL_NAME}"

mkdir -p "$LOGDIR"

# 54,576B（既定アセット）と 3B の 2 サイズを配信する www ディレクトリを作る。
WWW="$LOGDIR/www"
prepare_www() {
    mkdir -p "$WWW"
    cp -f "$ASSETS/www/index.html" "$WWW/index.html"
    printf 'abc' > "$WWW/3b.html"
}

cmd_up() {
    local cfg_name="${1:-h2c_proxy}" img="${2:-veil:glibc}"
    local cfgfile="$SCRIPT_DIR/configs/${cfg_name}.toml"
    [ -f "$cfgfile" ] || { echo "!! config が無い: $cfgfile" >&2; return 1; }

    prepare_www
    docker network create "$NET" >/dev/null 2>&1 || true
    docker rm -f "$VEIL_NAME" "$BACKEND_NAME" >/dev/null 2>&1

    docker run -d --rm --network "$NET" --network-alias perf-backend \
        -v "$SCRIPT_DIR/nginx/nginx-backend.conf:/etc/nginx/nginx.conf:ro" \
        -v "$WWW:/var/www:ro" \
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
        -v "$WWW:/var/www:ro" \
        --security-opt seccomp="$ASSETS/security/seccomp.json" \
        --name "$VEIL_NAME" "$img" >/dev/null || return 1

    local i
    for i in $(seq 1 40); do
        if docker run --rm --network "$NET" curlimages/curl:latest -s -o /dev/null \
             -w '%{http_code}' --http2-prior-knowledge "http://$VEIL_NAME:$H2C_PORT/" 2>/dev/null | grep -q 200; then
            echo "ready: $cfg_name ($img)"
            return 0
        fi
        sleep 0.5
    done
    echo "!! veil not ready" >&2
    docker logs "$VEIL_NAME" 2>&1 | tail -20 >&2
    return 1
}

# 比較対象 nginx（h2c: 静的 `/` と 逆プロキシ `/proxy/`）を同一ネットワークへ起動する。
cmd_upnginx() {
    prepare_www
    docker network create "$NET" >/dev/null 2>&1 || true
    docker rm -f "$NGINX_NAME" >/dev/null 2>&1
    docker run -d --rm --network "$NET" \
        -v "$SCRIPT_DIR/nginx/nginx.conf:/etc/nginx/nginx.conf:ro" \
        -v "$ASSETS/ssl:/etc/veil/ssl:ro" \
        -v "$WWW:/var/www:ro" \
        --name "$NGINX_NAME" nginx:alpine >/dev/null || return 1
    local i
    for i in $(seq 1 40); do
        if docker run --rm --network "$NET" curlimages/curl:latest -s -o /dev/null \
             -w '%{http_code}' --http2-prior-knowledge "http://$NGINX_NAME:$H2C_PORT/" 2>/dev/null | grep -q 200; then
            echo "ready: nginx"
            return 0
        fi
        sleep 0.5
    done
    echo "!! nginx not ready" >&2
    docker logs "$NGINX_NAME" 2>&1 | tail -20 >&2
    return 1
}

cmd_load() {
    local path="${1:-/}"; shift || true
    local args=("$@")
    [ ${#args[@]} -eq 0 ] && args=(-n 30000 -c 100 -m 10)
    docker run --rm --network "$NET" --entrypoint h2load "$H2_IMG" \
        "${args[@]}" "http://$TARGET:$H2C_PORT$path" 2>&1
}

# 負荷をかけながらクライアント / veil / 上流の 3 者の CPU を同時にサンプルする。
cmd_cpu() {
    local path="${1:-/}" secs="${2:-20}"
    local cli=lab-h2load-cpu
    docker rm -f "$cli" >/dev/null 2>&1
    docker run -d --rm --network "$NET" --name "$cli" --entrypoint h2load "$H2_IMG" \
        -c 100 -m 10 -D "$secs" "http://$TARGET:$H2C_PORT$path" >/dev/null
    sleep 3
    echo -e "sample\tclient\t$TARGET\tbackend"
    local i
    for i in $(seq 1 5); do
        # 3 コンテナを 1 回の docker stats 呼び出しで同時にサンプルする（時刻ずれを避ける）
        local line
        line=$(docker stats --no-stream --format '{{.Name}} {{.CPUPerc}}' "$cli" "$TARGET" "$BACKEND_NAME" 2>/dev/null \
            | awk -v c="$cli" -v v="$TARGET" -v b="$BACKEND_NAME" \
                '{gsub("%","",$2); s[$1]=$2} END {printf "%s\t%s\t%s", s[c], s[v], s[b]}')
        echo -e "$i\t$line"
    done
    docker logs "$cli" 2>&1 | grep -E 'finished in|requests:' || true
    docker rm -f "$cli" >/dev/null 2>&1
}

# veil の PID 名前空間へ入って syscall を集計する（ホストの ptrace_scope を迂回する）。
#
# **1 リクエストあたりの syscall 回数**を出すため、strace の観測窓が h2load の
# 実行全体を完全に覆うようにし、h2load が報告した完了リクエスト数で割る。
# 窓を時間で切ると「窓の中で何リクエスト処理したか」が分からず、比較できない。
cmd_strace() {
    local path="${1:-/}" nreq="${2:-20000}"
    local cli=lab-h2load-str straceout="$LOGDIR/strace.txt"
    docker rm -f "$cli" lab-strace >/dev/null 2>&1

    # ウォームアップ（接続確立・プール充填を観測窓の外へ出す）
    docker run --rm --network "$NET" --entrypoint h2load "$H2_IMG" \
        -n 2000 -c 100 -m 10 "http://$TARGET:$H2C_PORT$path" >/dev/null 2>&1

    # `--rm` を付けてはならない: stop 直後にコンテナごと消えて `docker logs`
    # （= strace の集計結果）が取れなくなる。明示的に rm する。
    # PID 名前空間内の**全プロセス**へアタッチするヘルパをファイルで渡す。
    # `-p 1 -f` だけでは、アタッチ時点で既に存在する子プロセス（nginx の worker 等）を
    # 拾えずマスタープロセスしか観測できず、集計が空になる。
    cat > "$LOGDIR/strace_all.sh" <<'STRACE_EOF'
#!/bin/sh
apk add --no-cache strace >/dev/null 2>&1
# 自分自身（exec 後に strace となる pid）を除外する。含めると strace は
# "I'm sorry, I can't let you do that, Dave." と言って何もせず終了する。
set --
for d in /proc/[0-9]*; do
    pid="${d#/proc/}"
    [ "$pid" = "$$" ] && continue
    set -- "$@" -p "$pid"
done
exec strace -c -f "$@" 2>&1
STRACE_EOF
    chmod +x "$LOGDIR/strace_all.sh"

    docker run -d --name lab-strace --pid="container:$TARGET" --cap-add SYS_PTRACE \
        -v "$LOGDIR/strace_all.sh:/strace_all.sh:ro" \
        --entrypoint /strace_all.sh alpine:latest >/dev/null

    # strace のアタッチ完了を待つ（apk add の分）
    sleep 12

    docker run --rm --network "$NET" --entrypoint h2load "$H2_IMG" \
        -n "$nreq" -c 100 -m 10 "http://$TARGET:$H2C_PORT$path" \
        > "$LOGDIR/strace_load.log" 2>&1

    docker stop -t 5 lab-strace >/dev/null 2>&1
    docker logs lab-strace > "$straceout" 2>&1 || true
    docker rm -f lab-strace >/dev/null 2>&1

    local done_req
    done_req=$(awk '/^requests:/ {print $4}' "$LOGDIR/strace_load.log")
    echo "== h2load =="
    grep -E 'finished in|^requests:' "$LOGDIR/strace_load.log"
    echo "== syscalls / request (完了 ${done_req} リクエスト) =="
    awk -v n="${done_req:-1}" '
        /^[[:space:]]*[0-9]/ {
            calls=$4; name=$NF;
            if (calls+0 > 0) printf "%-24s %12d %10.3f\n", name, calls, calls/n;
        }
        /^ *----/ {next}
    ' "$straceout" | sort -k2 -rn | head -25
    echo "(生ログ: $straceout)"
}

# 持続負荷中のメモリ推移を追う。
#
# io_uring の `IORING_OP_TIMEOUT`（`timeout()` の負け arm）は drop 時にキャンセルされず
# **満了まで（READ_TIMEOUT = 30 秒）カーネルと op テーブルに残る**ため、バックエンド脚で
# I/O ごとにタイマーを張る実装だと RSS が 30 秒かけて単調増加し、そこで頭打ちになる。
# 「増えて 30 秒で飽和する」形が見えれば、この蓄積が起きている強い証拠になる。
cmd_mem() {
    local path="${1:-/}" secs="${2:-70}"
    local cli=lab-h2load-mem
    docker rm -f "$cli" >/dev/null 2>&1
    echo -e "t_sec\tmem_mb"
    echo -e "0\t$(docker stats --no-stream --format '{{.MemUsage}}' "$TARGET" 2>/dev/null | awk '{print $1}')"
    docker run -d --rm --network "$NET" --name "$cli" --entrypoint h2load "$H2_IMG" \
        -c 100 -m 10 -D "$secs" "http://$TARGET:$H2C_PORT$path" >/dev/null
    local t=0
    while [ "$t" -lt "$secs" ]; do
        sleep 5
        t=$((t + 5))
        echo -e "$t\t$(docker stats --no-stream --format '{{.MemUsage}}' "$TARGET" 2>/dev/null | awk '{print $1}')"
    done
    docker rm -f "$cli" >/dev/null 2>&1
}

cmd_down() {
    docker rm -f "$VEIL_NAME" "$NGINX_NAME" "$BACKEND_NAME" lab-h2load-cpu lab-h2load-str >/dev/null 2>&1
    docker network rm "$NET" >/dev/null 2>&1
    echo "cleaned"
}

case "${1:-}" in
    up)     shift; cmd_up "$@" ;;
    upnginx) shift; cmd_upnginx "$@" ;;
    load)   shift; cmd_load "$@" ;;
    cpu)    shift; cmd_cpu "$@" ;;
    strace) shift; cmd_strace "$@" ;;
    mem)    shift; cmd_mem "$@" ;;
    down)   cmd_down ;;
    *) sed -n '2,20p' "${BASH_SOURCE[0]}"; exit 1 ;;
esac
