#!/usr/bin/env bash
# =============================================================================
# alloc_ab.sh — メモリアロケータ／アロケータ設定の交互 A/B（スループット + RSS 推移）
# =============================================================================
#
# F-183。veil をホストでネイティブ起動し（Docker イメージを作り直さずにバイナリと
# 環境変数だけを差し替えられる）、負荷は `local/h2load` コンテナを `--network host` で
# 別コアから掛ける。各ラウンドで全バリアントを交互に 1 回ずつ起動し、同じ負荷列を流す。
#
# 1 回の計測（バリアント × ラウンド）:
#   1. veil を起動（`taskset -c $VEIL_CPUS`、threads = $THREADS）→ 3 秒後のアイドル RSS
#   2. 負荷列（既定 5 種）を順に $DURATION 秒ずつ。各負荷の rps・失敗数・期間中ピーク RSS
#   3. 負荷後のアイドル RSS 推移（0 / 5 / 15 / 30 / 60 秒後）
#
# 使い方:
#   VARIANTS_FILE=variants.txt bash tools/perf/alloc_ab.sh
#
# variants.txt（1 行 1 バリアント。`#` 始まりは無視）:
#   <名前> <veil バイナリのパス> [ENV=VAL ...] [OPT:<設定キー>=<値> ...]（OPT は `veil -o` に渡す）
#   例:
#     mi_base   /path/to/veil-mimalloc
#     mi_purge  /path/to/veil-mimalloc MIMALLOC_PURGE_DELAY=100
#
# 主な環境変数:
#   ROUNDS (3)  DURATION (10)  THREADS (2)  VEIL_CPUS (0,1)  LOAD_CPUS (2,3)
#   WORKLOADS（既定は下の default_workloads）  IDLE_POINTS ("0 5 15 30 60")
#   OUT（結果 TSV。既定 tools/perf/results/alloc_ab/<日時>.tsv）
#
# 出力 TSV の列: variant round metric value
#   metric = idle_rss_kb | <wl>_rps | <wl>_failed | <wl>_peak_kb | after_<t>s_kb
# 集計: `python3 tools/perf/alloc_ab_summary.py <TSV>`
# =============================================================================
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
ASSETS="$REPO_ROOT/docker/assets"

VARIANTS_FILE="${VARIANTS_FILE:?VARIANTS_FILE を指定してください}"
ROUNDS="${ROUNDS:-3}"
DURATION="${DURATION:-10}"
THREADS="${THREADS:-2}"
VEIL_CPUS="${VEIL_CPUS:-0,1}"
LOAD_CPUS="${LOAD_CPUS:-2,3}"
IDLE_POINTS="${IDLE_POINTS:-0 5 15 30 60}"
H2_IMG="${H2_IMG:-local/h2load:latest}"
WRK_IMG="${WRK_IMG:-williamyeh/wrk:latest}"
# OpenSSL 3 リンクの wrk（TLS 1.3 チケット再開）。docker build -t local/wrk-openssl:latest tools/perf/wrk-openssl
WRK_OSSL_IMG="${WRK_OSSL_IMG:-local/wrk-openssl:latest}"
TLS_PORT="${TLS_PORT:-19443}"
H2C_PORT="${H2C_PORT:-19080}"
UP_PORT="${UP_PORT:-19081}"
STAMP="$(date +%Y%m%d-%H%M%S)"
WORK="${WORK:-$SCRIPT_DIR/results/alloc_ab/work}"
OUT="${OUT:-$SCRIPT_DIR/results/alloc_ab/$STAMP.tsv}"
UP_NAME=alloc-ab-upstream

mkdir -p "$WORK/www" "$WORK/ssl" "$(dirname "$OUT")"

log() { printf '[alloc_ab] %s\n' "$*" >&2; }

# 負荷の定義: 名前 → h2load の引数（URL は最後）。
#   h1tls_*  : HTTP/1.1 over TLS（静的配信）。接続数を多めにして接続あたりメモリを効かせる
#   h2tls_*  : HTTP/2 over TLS（静的配信、多重化）
#   h1prx_*  : HTTP/1.1 over TLS → 平文上流（プロキシ）
#   h2cprx_* : h2c → 平文上流（プロキシ）
#   h1tls_hs : HTTP/1.1 over TLS、要求ごとに接続を閉じる（接続チャーン。LibreSSL 2.7 の wrk =
#              TLS 1.2 の session ID 再開）
#   h1tls_hs13 : 同上を OpenSSL 3 の wrk で（TLS 1.3 のチケット再開。local/wrk-openssl）
default_workloads() {
    cat <<EOF
h1tls_54k --h1 -t2 -c256 https://127.0.0.1:${TLS_PORT}/
h1tls_3b --h1 -t2 -c256 https://127.0.0.1:${TLS_PORT}/3b.html
h2tls_3b -t2 -c32 -m16 https://127.0.0.1:${TLS_PORT}/3b.html
h1prx_3b --h1 -t2 -c128 https://127.0.0.1:${TLS_PORT}/p/3b.html
h2cprx_54k -t2 -c32 -m8 http://127.0.0.1:${H2C_PORT}/p/
h1tls_hs wrk -t2 -c64 https://127.0.0.1:${TLS_PORT}/3b.html
h1tls_hs13 wrkossl -t2 -c64 https://127.0.0.1:${TLS_PORT}/3b.html
EOF
}
WORKLOADS="${WORKLOADS:-$(default_workloads)}"

prepare() {
    cp -f "$ASSETS/www/index.html" "$WORK/www/index.html"
    printf 'abc' > "$WORK/www/3b.html"
    cp -f "$ASSETS/ssl/cert.pem" "$ASSETS/ssl/key.pem" "$WORK/ssl/"
    cat > "$WORK/nginx-up.conf" <<EOF
worker_processes 1;
events { worker_connections 8192; }
http {
    access_log off;
    error_log /dev/stderr warn;
    sendfile on;
    keepalive_timeout 65;
    keepalive_requests 1000000;
    server {
        listen ${UP_PORT};
        root /var/www;
        location / { index index.html; }
    }
}
EOF
    cat > "$WORK/veil.toml" <<EOF
[server]
listen = "127.0.0.1:${TLS_PORT}"
http2_enabled = true
listen_cleartext = "127.0.0.1:${H2C_PORT}"
enable_secure_redirect = false
threads = ${THREADS}

[logging]
level = "warn"

[security]
allow_security_failures = false
enable_seccomp = true
enable_landlock = true
seccomp_mode = "filter"
landlock_read_paths = ["${WORK}", "/etc/resolv.conf", "/etc/hosts", "/etc/nsswitch.conf", "/lib", "/lib64", "/usr"]
landlock_write_paths = []

[performance]
huge_pages_enabled = false

[tls]
cert_path = "${WORK}/ssl/cert.pem"
key_path = "${WORK}/ssl/key.pem"
ktls_enabled = false

[upstreams."up"]
algorithm = "round_robin"
servers = ["http://127.0.0.1:${UP_PORT}/"]

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

start_upstream() {
    docker rm -f "$UP_NAME" >/dev/null 2>&1
    docker run -d --rm --network host --cpuset-cpus "${LOAD_CPUS##*,}" \
        -v "$WORK/nginx-up.conf:/etc/nginx/nginx.conf:ro" -v "$WORK/www:/var/www:ro" \
        --name "$UP_NAME" nginx:alpine >/dev/null
}

rss_kb() { awk '/^VmRSS:/ {print $2}' "/proc/$1/status" 2>/dev/null || echo 0; }

# veil の累積 CPU 時間（user + system、マイクロ秒。/proc/<pid>/stat の 14・15 列）。
CLK_TCK="$(getconf CLK_TCK)"
cpu_us() { awk -v hz="$CLK_TCK" '{ printf "%d", ($14 + $15) * 1000000 / hz }' "/proc/$1/stat" 2>/dev/null || echo 0; }

wait_port() {
    local port="$1" i
    for i in $(seq 1 100); do
        (exec 3<>"/dev/tcp/127.0.0.1/$port") 2>/dev/null && return 0
        sleep 0.1
    done
    return 1
}

VEIL_PID=""
# 引数: バイナリ, ENV=VAL..., 設定上書き（`-o` に渡す `OPT:<key>=<value>`。例 `OPT:logging.channel_size=16384`）
start_veil() {
    local bin="$1"; shift
    local envs=() opts=() a
    for a in "$@"; do
        case "$a" in
            OPT:*) opts+=(-o "${a#OPT:}") ;;
            *) envs+=("$a") ;;
        esac
    done
    env "${envs[@]}" taskset -c "$VEIL_CPUS" "$bin" -c "$WORK/veil.toml" "${opts[@]}" >"$WORK/veil.log" 2>&1 &
    VEIL_PID=$!
    wait_port "$TLS_PORT" && wait_port "$H2C_PORT"
}

stop_veil() {
    [ -n "$VEIL_PID" ] && kill "$VEIL_PID" >/dev/null 2>&1
    wait "$VEIL_PID" 2>/dev/null
    VEIL_PID=""
}

# 負荷 1 本: rps・失敗数を返し、期間中の RSS ピークを記録する。
# 引数の先頭が `wrk` なら wrk（`-H "Connection: close"` で要求ごとに TLS ハンドシェイクを
# させる接続チャーン計測用）、それ以外は h2load。
run_load() {
    local name="$1"; shift
    local peak=0 r logf="$WORK/load_${name}.log" tool=h2load
    if [ "${1:-}" = "wrk" ] || [ "${1:-}" = "wrkossl" ]; then
        local img="$WRK_IMG"
        [ "$1" = "wrkossl" ] && img="$WRK_OSSL_IMG"
        tool=wrk; shift
        docker run --rm --network host --cpuset-cpus "$LOAD_CPUS" "$img" \
            -d "${DURATION}s" -H "Connection: close" "$@" >"$logf" 2>&1 &
    else
        docker run --rm --network host --cpuset-cpus "$LOAD_CPUS" --entrypoint h2load "$H2_IMG" \
            -D "$DURATION" --warm-up-time 1 "$@" >"$logf" 2>&1 &
    fi
    local lp=$!
    local c0
    c0=$(cpu_us "$VEIL_PID")
    while kill -0 "$lp" 2>/dev/null; do
        r=$(rss_kb "$VEIL_PID"); [ "${r:-0}" -gt "$peak" ] && peak=$r
        sleep 0.25
    done
    wait "$lp"
    local c1 rps failed nreq
    c1=$(cpu_us "$VEIL_PID")
    if [ "$tool" = wrk ]; then
        rps=$(awk '/^Requests\/sec:/ {print $2}' "$logf")
        nreq=$(awk '/requests in/ {print $1}' "$logf")
        # Socket errors（connect/read/write/timeout）と非 2xx/3xx
        failed=$(awk '/Socket errors:/ {gsub(",","");f=$4+$6+$8+$10} /Non-2xx or 3xx/ {s=$5} END {print f+s}' "$logf")
        echo "${rps:-0} ${failed:-0} $peak $(awk -v a="$c0" -v b="$c1" -v n="${nreq:-0}" 'BEGIN { if (n > 0) printf "%.2f", (b - a) / n; else print 0 }')"
        return
    fi
    nreq=$(awk '/^requests:/ {print $8}' "$logf")
    rps=$(awk '/^finished in/ {gsub(",","",$4); print $4}' "$logf")
    # 失敗 = failed + errored + 4xx/5xx（h2load の failed は非 2xx を数えない）
    failed=$(awk '/^requests:/ {f=$10+$12} /^status codes:/ {s=$7+$9} END {print f+s}' "$logf")
    echo "${rps:-0} ${failed:-NA} $peak $(awk -v a="$c0" -v b="$c1" -v n="${nreq:-0}" 'BEGIN { if (n > 0) printf "%.2f", (b - a) / n; else print 0 }')"
}

cleanup() {
    stop_veil
    docker rm -f "$UP_NAME" >/dev/null 2>&1
}
trap cleanup EXIT INT TERM

# 前回の残骸（別名のバイナリは `pkill -x veil` に掛からない）が SO_REUSEPORT で同じポートを
# 掴んでいると、要求の一部が古い設定のプロセスへ流れて計測が壊れる。
if (exec 3<>"/dev/tcp/127.0.0.1/$TLS_PORT") 2>/dev/null; then
    log "!! ポート $TLS_PORT が既に使われている（残存プロセスを止めること）"; exit 1
fi

prepare
start_upstream
wait_port "$UP_PORT" || { log "upstream nginx が起動しない"; exit 1; }
printf 'variant\tround\tmetric\tvalue\n' > "$OUT"
log "結果: $OUT"

mapfile -t VARIANT_LINES < <(grep -v '^[[:space:]]*#' "$VARIANTS_FILE" | grep -v '^[[:space:]]*$')

for round in $(seq 1 "$ROUNDS"); do
    for line in "${VARIANT_LINES[@]}"; do
        read -r -a f <<<"$line"
        name="${f[0]}"; bin="${f[1]}"; envs=("${f[@]:2}")
        log "round $round / $name"
        if ! start_veil "$bin" "${envs[@]}"; then
            log "!! $name が起動しない（$WORK/veil.log）"; stop_veil; continue
        fi
        sleep 3
        printf '%s\t%s\tidle_rss_kb\t%s\n' "$name" "$round" "$(rss_kb "$VEIL_PID")" >> "$OUT"
        while read -r wl args; do
            [ -z "$wl" ] && continue
            # shellcheck disable=SC2086
            read -r rps failed peak cpr <<<"$(run_load "$wl" $args)"
            printf '%s\t%s\t%s_rps\t%s\n%s\t%s\t%s_failed\t%s\n%s\t%s\t%s_peak_kb\t%s\n%s\t%s\t%s_cpu_us_per_req\t%s\n' \
                "$name" "$round" "$wl" "$rps" "$name" "$round" "$wl" "$failed" \
                "$name" "$round" "$wl" "$peak" "$name" "$round" "$wl" "$cpr" >> "$OUT"
            log "  $wl rps=$rps failed=$failed peak=${peak}KB cpu/req=${cpr}us"
        done <<<"$WORKLOADS"
        t0=$(date +%s)
        for t in $IDLE_POINTS; do
            while [ $(( $(date +%s) - t0 )) -lt "$t" ]; do sleep 0.5; done
            printf '%s\t%s\tafter_%ss_kb\t%s\n' "$name" "$round" "$t" "$(rss_kb "$VEIL_PID")" >> "$OUT"
        done
        log "  after-idle: $(grep -P "^$name\t$round\tafter_" "$OUT" | awk '{printf "%s=%s ", $3, $4}')"
        stop_veil
    done
done

docker rm -f "$UP_NAME" >/dev/null 2>&1
python3 "$SCRIPT_DIR/alloc_ab_summary.py" "$OUT"
