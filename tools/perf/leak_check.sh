#!/usr/bin/env bash
# =============================================================================
# leak_check.sh — 接続・異常切断・設定リロードの反復でメモリ・fd が増え続けないかの試験（F-185）
# =============================================================================
#
# veil を 1 プロセス起動したまま、シナリオごとに「同じ操作 1 回分（サイクル）→ 静定待ち →
# 採取」を CYCLES 回繰り返す。リークがあればサイクルごとに一定量ずつ増えるので、
# 採取値をサイクル番号に回帰した傾き（1 サイクルあたりの増分）で判定する。
# 先頭 WARMUP サイクルはプール・キャッシュ・セッションチケット等が満ちるまでの区間として除外する。
#
# 採取する値（静定後）:
#   rss_kb / anon_kb   VmRSS / RssAnon（/proc/<pid>/status）
#   live_bytes         生存ヒープのバイト数（確保 − 解放。`alloc-stats` ビルドの veil_alloc_live_bytes）。
#                      アロケータの保持・返却の揺れを含まない「生きているヒープ」で、リーク判定の本命
#   live_allocs        生存しているアロケーションの数（同 veil_alloc_live_allocs）
#   heap_active        veil-alloc の使用中スロットのバイト数（veil_allocator_bytes{state="active"}）
#   segments / heaps   veil-alloc のセグメント数・ヒープ数（スレッドのヒープが積み上がらないか）
#   fds / threads      /proc/<pid>/fd の数・スレッド数（fd・スレッドのリーク）
#   conns              アクティブ接続数のメトリクス（接続カウンタの戻し忘れ）
#
# シナリオ（SCENARIOS で選択。既定は全部）:
#   h1_churn    HTTP/1.1 TLS: 1 接続 1 要求で CHURN_CONNS 本（静的 54KB / 3B / キャッシュ / プロキシ）
#   h2_churn    HTTP/2 TLS: 1 接続 1 要求で CHURN_CONNS 本
#   h2c_churn   h2c → 上流プロキシ: 1 接続 1 要求で CHURN_CONNS 本
#   h3_churn    HTTP/3: 1 接続 1 要求で H3_CONNS 本（local/h2load-h3 が無ければスキップ）
#   abort       異常切断（ハンドシェイク途中・要求途中・応答途中の RST。leak_abort_client.py）
#   reload      SIGHUP を RELOADS 回（設定 a/b を交互に書き換え、証明書の mtime も更新）。
#               リロード中も軽い負荷を流す。ログの成功行の数で「本当にリロードしたか」を確かめる
#
# 使い方:
#   cargo build --release --features full,alloc-stats   # live_bytes / live_allocs を採るため
#   BIN=target/release/veil bash tools/perf/leak_check.sh
#   集計: python3 tools/perf/leak_check_summary.py <OUT の TSV>
#
# 環境変数: BIN（必須） CYCLES (30) WARMUP (5) SETTLE_SECS (5) THREADS (2) VEIL_CPUS (0,1)
#           LOAD_CPUS (2,3) CHURN_CONNS (1000) H3_CONNS (300) ABORT_N（種類ごと 100） RELOADS (10)
#           SCENARIOS（既定 "h1_churn h2_churn h2c_churn h3_churn abort reload"） OUT
# =============================================================================
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
ASSETS="$REPO_ROOT/docker/assets"

BIN="${BIN:?BIN（veil バイナリ）を指定してください}"
BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")"
CYCLES="${CYCLES:-30}"
WARMUP="${WARMUP:-5}"
SETTLE_SECS="${SETTLE_SECS:-5}"
THREADS="${THREADS:-2}"
VEIL_CPUS="${VEIL_CPUS:-0,1}"
LOAD_CPUS="${LOAD_CPUS:-2,3}"
CHURN_CONNS="${CHURN_CONNS:-1000}"
H3_CONNS="${H3_CONNS:-300}"
ABORT_N="${ABORT_N:-100}"
RELOADS="${RELOADS:-10}"
SCENARIOS="${SCENARIOS:-h1_churn h2_churn h2c_churn h3_churn abort reload}"
H2_IMG="${H2_IMG:-local/h2load:latest}"
H3_IMG="${H3_IMG:-local/h2load-h3:latest}"
TLS_PORT="${TLS_PORT:-19443}"
H2C_PORT="${H2C_PORT:-19080}"
UP_PORT="${UP_PORT:-19081}"
STAMP="$(date +%Y%m%d-%H%M%S)"
WORK="${WORK:-$SCRIPT_DIR/results/leak_check/work}"
OUT="${OUT:-$SCRIPT_DIR/results/leak_check/$STAMP.tsv}"
UP_NAME=leak-check-upstream

log() { printf '[leak_check] %s\n' "$*" >&2; }

mkdir -p "$WORK/www/c" "$WORK/ssl" "$WORK/logs" "$(dirname "$OUT")"

# 設定 a/b: リロードのたびに中身が変わるようにルート・上流の振り分け・アクセスログの出力先を変える
# （アクセスログの出力先が変わると書き込みスレッドを作り直す経路を通る）。
write_config() { # variant(a|b) path
    local v="$1" algo extra
    if [ "$v" = a ]; then
        algo=round_robin
        extra=""
    else
        algo=least_conn
        extra=$'[[route]]\n[route.conditions]\npath = "/b/*"\n[route.action]\ntype = "Proxy"\nupstream = "up"\n'
    fi
    cat > "$2" <<EOF
[server]
listen = "127.0.0.1:${TLS_PORT}"
http2_enabled = true
listen_cleartext = "127.0.0.1:${H2C_PORT}"
enable_secure_redirect = false
threads = ${THREADS}

[logging]
level = "info"

[security]
allow_security_failures = false
enable_seccomp = true
enable_landlock = true
seccomp_mode = "filter"
landlock_read_paths = ["${WORK}", "/etc/resolv.conf", "/etc/hosts", "/etc/nsswitch.conf", "/lib", "/lib64", "/usr"]
landlock_write_paths = ["${WORK}/logs"]

[performance]
huge_pages_enabled = false

[tls]
cert_path = "${WORK}/ssl/cert.pem"
key_path = "${WORK}/ssl/key.pem"
ktls_enabled = false
auto_reload = true
reload_interval_secs = 3600

[http3]
enabled = true
listen = "127.0.0.1:${TLS_PORT}"

[access_log]
enabled = true
format = "json"
file_path = "${WORK}/logs/access-${v}.log"

[prometheus]
enabled = true
allowed_ips = ["127.0.0.1"]

[upstreams."up"]
algorithm = "${algo}"
servers = ["http://127.0.0.1:${UP_PORT}/"]

${extra}
[[route]]
[route.conditions]
path = "/p/*"
[route.action]
type = "Proxy"
upstream = "up"
[route.security]
allowed_methods = ["HEAD", "GET"]

[[route]]
[route.conditions]
path = "/c/*"
[route.action]
type = "File"
path = "${WORK}/www/c/"
[route.cache]
enabled = true
default_ttl_secs = 60
methods = ["GET"]
cacheable_statuses = [200]

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "${WORK}/www/"
[route.security]
allowed_methods = ["HEAD", "GET"]
[route.open_file_cache]
enabled = true
valid_duration_secs = 60
max_entries = 1024
EOF
}

prepare() {
    cp -f "$ASSETS/www/index.html" "$WORK/www/index.html"
    cp -f "$ASSETS/www/index.html" "$WORK/www/c/index.html"
    printf 'abc' > "$WORK/www/3b.html"
    cp -f "$ASSETS/ssl/cert.pem" "$ASSETS/ssl/key.pem" "$WORK/ssl/"
    rm -f "$WORK/logs/"*.log
    write_config a "$WORK/veil.a.toml"
    write_config b "$WORK/veil.b.toml"
    cp -f "$WORK/veil.a.toml" "$WORK/veil.toml"
    cat > "$WORK/nginx-up.conf" <<EOF
worker_processes 1;
events { worker_connections 8192; }
http {
    access_log off;
    error_log /dev/stderr warn;
    keepalive_timeout 65;
    keepalive_requests 1000000;
    server {
        listen ${UP_PORT};
        root /var/www;
        location / { index index.html; }
    }
}
EOF
}

wait_port() {
    local port="$1" i
    for i in $(seq 1 100); do
        (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null && return 0
        sleep 0.1
    done
    return 1
}

VPID=""
cleanup() {
    [ -n "$VPID" ] && kill "$VPID" 2>/dev/null
    docker rm -f "$UP_NAME" >/dev/null 2>&1
}
trap cleanup EXIT
trap 'cleanup; exit 130' INT TERM

metric() { # name label-filter（例: 'state="active"'）
    awk -v n="$1" -v f="$2" '$1 ~ "^"n"($|[{])" && index($1, f) { s += $2 } END { printf "%d", s }' "$WORK/metrics.txt"
}

sample() { # scenario cycle
    curl -sk --max-time 5 "https://127.0.0.1:${TLS_PORT}/__metrics" > "$WORK/metrics.txt" 2>/dev/null
    local rss anon fds thr
    rss=$(awk '/^VmRSS:/ {print $2}' "/proc/$VPID/status")
    anon=$(awk '/^RssAnon:/ {print $2}' "/proc/$VPID/status")
    thr=$(awk '/^Threads:/ {print $2}' "/proc/$VPID/status")
    fds=$(find "/proc/$VPID/fd" -mindepth 1 -maxdepth 1 2>/dev/null | wc -l)
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$1" "$2" "$rss" "$anon" \
        "$(metric veil_alloc_live_bytes '')" "$(metric veil_alloc_live_allocs '')" \
        "$(metric veil_allocator_bytes 'state="active"')" \
        "$(metric veil_allocator_count 'kind="segments"')" \
        "$(metric veil_allocator_count 'kind="heaps"')" \
        "$fds" "$thr" "$(metric veil_proxy_http_active_connections '')" >> "$OUT"
}

h2load() { # 引数は h2load にそのまま
    docker run --rm --network host --cpuset-cpus "$LOAD_CPUS" --entrypoint h2load "$H2_IMG" "$@" 2>&1
}

# h2load の結果から成功要求数を取る（失敗・エラーは別途ログに残す）
succeeded() { awk '/^requests:/ { for (i = 1; i <= NF; i++) if ($(i + 1) == "succeeded,") { print $i; exit } }'; }

run_cycle() { # scenario
    local s="$1" r
    case "$s" in
        h1_churn)
            # 1 接続 1 要求（-n = -c）。静的 54KB / 3B / キャッシュ / プロキシを 4 分の 1 ずつ
            local q=$((CHURN_CONNS / 4)) u
            for u in / /3b.html /c/ /p/3b.html; do
                r=$(h2load --h1 -n "$q" -c "$q" "https://127.0.0.1:${TLS_PORT}${u}" | succeeded)
                echo "h1 $u ok=$r/$q" >> "$WORK/cycle.log"
            done ;;
        h2_churn)
            local q=$((CHURN_CONNS / 2)) u
            for u in / /p/3b.html; do
                r=$(h2load -n "$q" -c "$q" "https://127.0.0.1:${TLS_PORT}${u}" | succeeded)
                echo "h2 $u ok=$r/$q" >> "$WORK/cycle.log"
            done ;;
        h2c_churn)
            r=$(h2load -n "$CHURN_CONNS" -c "$CHURN_CONNS" "http://127.0.0.1:${H2C_PORT}/p/" | succeeded)
            echo "h2c ok=$r/$CHURN_CONNS" >> "$WORK/cycle.log" ;;
        h3_churn)
            r=$(docker run --rm --network host --cpuset-cpus "$LOAD_CPUS" --entrypoint h2load "$H3_IMG" \
                --alpn-list=h3 -n "$H3_CONNS" -c "$H3_CONNS" "https://127.0.0.1:${TLS_PORT}/" 2>&1 | succeeded)
            echo "h3 ok=$r/$H3_CONNS" >> "$WORK/cycle.log" ;;
        abort)
            r=$(taskset -c "$LOAD_CPUS" python3 "$SCRIPT_DIR/leak_abort_client.py" "$TLS_PORT" "$H2C_PORT" "$ABORT_N" 16)
            echo "abort $r" >> "$WORK/cycle.log" ;;
        reload)
            # 軽い負荷を流しながら（リロード中の要求が古い設定を握ったままになる経路）SIGHUP を送る。
            # リロードスレッドは 500ms 周期でフラグを見るので、間隔はそれより長くする
            h2load -D $((RELOADS + 2)) -c 8 -m 4 "https://127.0.0.1:${TLS_PORT}/p/3b.html" > "$WORK/reload-load.log" &
            local lp=$! i
            for i in $(seq 1 "$RELOADS"); do
                RELOAD_SEQ=$((RELOAD_SEQ + 1))
                if [ $((RELOAD_SEQ % 2)) = 1 ]; then
                    cp -f "$WORK/veil.b.toml" "$WORK/veil.toml"
                else
                    cp -f "$WORK/veil.a.toml" "$WORK/veil.toml"
                fi
                touch "$WORK/ssl/cert.pem" "$WORK/ssl/key.pem"
                kill -HUP "$VPID"
                sleep 0.8
            done
            wait "$lp"
            echo "reload load ok=$(succeeded < "$WORK/reload-load.log")" >> "$WORK/cycle.log" ;;
    esac
}

prepare
docker rm -f "$UP_NAME" >/dev/null 2>&1
docker run -d --rm --network host --cpuset-cpus "${LOAD_CPUS##*,}" \
    -v "$WORK/nginx-up.conf:/etc/nginx/nginx.conf:ro" -v "$WORK/www:/var/www:ro" \
    --name "$UP_NAME" nginx:alpine >/dev/null

taskset -c "$VEIL_CPUS" "$BIN" -c "$WORK/veil.toml" > "$WORK/veil.log" 2>&1 &
VPID=$!
wait_port "$TLS_PORT" && wait_port "$H2C_PORT" || { log "veil が起動しない（$WORK/veil.log）"; exit 1; }
sleep 2
curl -sk "https://127.0.0.1:${TLS_PORT}/__metrics" | grep -q veil_allocator_bytes \
    || log "警告: veil_allocator_bytes が無い（veil-alloc ビルドでない）。heap_active 列は 0 になる"
curl -sk "https://127.0.0.1:${TLS_PORT}/__metrics" | grep -q veil_alloc_live_bytes \
    || log "警告: veil_alloc_live_bytes が無い（alloc-stats ビルドでない）。live_* 列は 0 になる"

: > "$WORK/cycle.log"
printf 'scenario\tcycle\trss_kb\tanon_kb\tlive_bytes\tlive_allocs\theap_active\tsegments\theaps\tfds\tthreads\tconns\n' > "$OUT"
sample start 0
RELOAD_SEQ=0
for s in $SCENARIOS; do
    if [ "$s" = h3_churn ] && ! docker image inspect "$H3_IMG" >/dev/null 2>&1; then
        log "$H3_IMG が無いので h3_churn をスキップ"; continue
    fi
    log "== $s（$CYCLES サイクル、先頭 $WARMUP は除外）"
    for c in $(seq 1 "$CYCLES"); do
        echo "# $s $c" >> "$WORK/cycle.log"
        run_cycle "$s"
        kill -0 "$VPID" 2>/dev/null || { log "veil が終了した（$WORK/veil.log）"; exit 1; }
        sleep "$SETTLE_SECS"
        sample "$s" "$c"
    done
    tail -1 "$OUT" | sed 's/^/[leak_check]   /' >&2
done

# 成功時は config.rs と server.rs の 2 行が出るので、リロードスレッド（server.rs）の行だけ数える
ok=$(grep -c "server.rs.*Configuration reloaded successfully" "$WORK/veil.log")
tls=$(grep -c "TLS certificate reloaded via SIGHUP" "$WORK/veil.log")
log "設定リロード成功 ${ok} 回・証明書リロード ${tls} 回（送った SIGHUP ${RELOAD_SEQ} 回）"
log "結果: $OUT（サイクルの詳細: $WORK/cycle.log）"
python3 "$SCRIPT_DIR/leak_check_summary.py" "$OUT" "$WARMUP"
