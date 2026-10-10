#!/bin/sh
# =============================================================================
# alloc_ab_bsd.sh — BSD ゲスト内のアロケータ設定の交互 A/B（スループット + RSS 推移、F-183）
# =============================================================================
#
# tools/perf/alloc_ab.sh（Linux、Docker で負荷生成）の BSD 版。ゲスト内 loopback で
# veil を `cpuset -l $VEIL_CPUS` に固定して起動し、wrk / h2load を残りのコアから掛ける。
# 各ラウンドで全バリアントを交互に 1 回ずつ起動し、同じ負荷列を流す。
#
# 実行場所 : FreeBSD ゲスト内（root）。前提: pkg install nginx nghttp2 wrk curl
#
# 使い方:
#   VARIANTS_FILE=variants.txt sh tools/perf/bsd/alloc_ab_bsd.sh
#
# variants.txt（1 行 1 バリアント。`#` 始まりは無視。値に空白を含めないこと）:
#   <名前> <veil バイナリ> [ENV=VAL ...] [OPT:<設定キー>=<値> ...]
#   例:
#     je_default ./target/release/veil MALLOC_CONF=dirty_decay_ms:10000
#     je_veil    ./target/release/veil
#
# 環境変数: ROUNDS (3) DURATION (10) THREADS (2) VEIL_CPUS (0,1) LOAD_CPUS (2,3)
#           IDLE_POINTS ("0 5 15 30") OUT（既定 /tmp/alloc_ab_bsd/<日時>.tsv）
# 出力 TSV の列: variant round metric value（tools/perf/alloc_ab_summary.py で集計できる）
# =============================================================================
set -u

REPO="$(cd "$(dirname "$0")/../../.." && pwd)"
VARIANTS_FILE="${VARIANTS_FILE:?VARIANTS_FILE を指定してください}"
ROUNDS="${ROUNDS:-3}"
DURATION="${DURATION:-10}"
THREADS="${THREADS:-2}"
VEIL_CPUS="${VEIL_CPUS:-0,1}"
LOAD_CPUS="${LOAD_CPUS:-2,3}"
IDLE_POINTS="${IDLE_POINTS:-0 5 15 30}"
TLS_PORT="${TLS_PORT:-19443}"
H2C_PORT="${H2C_PORT:-19080}"
UP_PORT="${UP_PORT:-19081}"
WORK="${WORK:-/tmp/alloc_ab_bsd}"
OUT="${OUT:-$WORK/$(date +%Y%m%d-%H%M%S).tsv}"

mkdir -p "$WORK/www" "$WORK/ssl"
log() { echo "[alloc_ab_bsd] $*" >&2; }

cp -f "$REPO/docker/assets/www/index.html" "$WORK/www/index.html"
printf 'abc' > "$WORK/www/3b.html"
if [ -f "$REPO/docker/assets/ssl/cert.pem" ]; then
    cp -f "$REPO/docker/assets/ssl/cert.pem" "$REPO/docker/assets/ssl/key.pem" "$WORK/ssl/"
elif [ ! -f "$WORK/ssl/cert.pem" ]; then
    # 証明書は git 管理外。無ければ自己署名（P-256）を作る。
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 30 \
        -subj /CN=localhost -keyout "$WORK/ssl/key.pem" -out "$WORK/ssl/cert.pem" >/dev/null 2>&1
fi

cat > "$WORK/nginx-up.conf" <<EOF
worker_processes 1;
pid $WORK/nginx.pid;
events { worker_connections 8192; }
http {
    access_log off;
    error_log $WORK/nginx-error.log warn;
    sendfile on;
    keepalive_requests 1000000;
    server { listen 127.0.0.1:${UP_PORT}; root $WORK/www; location / { index index.html; } }
}
EOF

cat > "$WORK/veil.toml" <<EOF
[server]
listen = "127.0.0.1:${TLS_PORT}"
http2_enabled = true
h2c_enabled = true
h2c_listen = "127.0.0.1:${H2C_PORT}"
threads = ${THREADS}

[logging]
level = "warn"

[security]
allow_security_failures = true

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

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "${WORK}/www/"
[route.open_file_cache]
enabled = true
valid_duration_secs = 60
max_entries = 1024
EOF

# 負荷の定義: 名前 ツール 引数...（URL は最後）
WORKLOADS="${WORKLOADS:-h1tls_54k wrk -t2 -c256 https://127.0.0.1:${TLS_PORT}/
h1tls_3b wrk -t2 -c256 https://127.0.0.1:${TLS_PORT}/3b.html
h2tls_3b h2load -t2 -c32 -m16 https://127.0.0.1:${TLS_PORT}/3b.html
h1prx_3b wrk -t2 -c128 https://127.0.0.1:${TLS_PORT}/p/3b.html
h2cprx_54k h2load -t2 -c32 -m8 http://127.0.0.1:${H2C_PORT}/p/
h1tls_hs wrkclose -t2 -c64 https://127.0.0.1:${TLS_PORT}/3b.html}"

rss_kb() { ps -o rss= -p "$1" 2>/dev/null | tr -d ' ' || echo 0; }

wait_port() {
    i=0
    while [ $i -lt 100 ]; do
        curl -sk -o /dev/null "$1" 2>/dev/null && return 0
        sleep 0.1; i=$((i + 1))
    done
    return 1
}

nginx -c "$WORK/nginx-up.conf" 2>/dev/null || log "nginx 起動失敗（既に起動済みなら無視）"
cleanup() { kill $(cat "$WORK/nginx.pid" 2>/dev/null) 2>/dev/null; [ -n "${VPID:-}" ] && kill "$VPID" 2>/dev/null; }
trap cleanup EXIT
trap 'cleanup; exit 130' INT TERM

printf 'variant\tround\tmetric\tvalue\n' > "$OUT"
log "結果: $OUT"

r=1
while [ "$r" -le "$ROUNDS" ]; do
    grep -v '^[[:space:]]*#' "$VARIANTS_FILE" | grep -v '^[[:space:]]*$' > "$WORK/variants.cur"
    while read -r name bin rest; do
        envs=""; opts=""
        for a in $rest; do
            case "$a" in
                OPT:*) opts="$opts -o ${a#OPT:}" ;;
                *) envs="$envs $a" ;;
            esac
        done
        log "round $r / $name"
        # shellcheck disable=SC2086
        env $envs cpuset -l "$VEIL_CPUS" "$bin" -c "$WORK/veil.toml" $opts > "$WORK/veil.log" 2>&1 &
        VPID=$!
        if ! wait_port "https://127.0.0.1:${TLS_PORT}/3b.html"; then
            log "!! $name が起動しない（$WORK/veil.log）"; kill "$VPID" 2>/dev/null; VPID=""; continue
        fi
        sleep 3
        printf '%s\t%s\tidle_rss_kb\t%s\n' "$name" "$r" "$(rss_kb "$VPID")" >> "$OUT"
        echo "$WORKLOADS" | while read -r wl tool args; do
            [ -z "$wl" ] && continue
            logf="$WORK/load_$wl.log"
            case "$tool" in
                wrk) cpuset -l "$LOAD_CPUS" wrk -d "${DURATION}s" $args > "$logf" 2>&1 & ;;
                wrkclose) cpuset -l "$LOAD_CPUS" wrk -d "${DURATION}s" -H "Connection: close" $args > "$logf" 2>&1 & ;;
                h2load) cpuset -l "$LOAD_CPUS" h2load -D "$DURATION" --warm-up-time 1 $args > "$logf" 2>&1 & ;;
            esac
            lp=$!; peak=0
            while kill -0 "$lp" 2>/dev/null; do
                x=$(rss_kb "$VPID"); [ "${x:-0}" -gt "$peak" ] && peak=$x
                sleep 0.25
            done
            wait "$lp"
            case "$tool" in
                wrk*)
                    rps=$(awk '/^Requests\/sec:/ {print $2}' "$logf")
                    failed=$(awk '/Socket errors:/ {gsub(",","");f=$4+$6+$8+$10} /Non-2xx or 3xx/ {s=$5} END {print f+s}' "$logf") ;;
                h2load)
                    rps=$(awk '/^finished in/ {gsub(",","",$4); print $4}' "$logf")
                    failed=$(awk '/^requests:/ {f=$10+$12} /^status codes:/ {s=$7+$9} END {print f+s}' "$logf") ;;
            esac
            printf '%s\t%s\t%s_rps\t%s\n%s\t%s\t%s_failed\t%s\n%s\t%s\t%s_peak_kb\t%s\n' \
                "$name" "$r" "$wl" "${rps:-0}" "$name" "$r" "$wl" "${failed:-NA}" "$name" "$r" "$wl" "$peak" >> "$OUT"
            log "  $wl rps=${rps:-0} failed=${failed:-NA} peak=${peak}KB"
        done
        t0=$(date +%s)
        for t in $IDLE_POINTS; do
            while [ $(( $(date +%s) - t0 )) -lt "$t" ]; do sleep 0.5; done
            printf '%s\t%s\tafter_%ss_kb\t%s\n' "$name" "$r" "$t" "$(rss_kb "$VPID")" >> "$OUT"
        done
        kill "$VPID" 2>/dev/null; wait "$VPID" 2>/dev/null; VPID=""
        sleep 0.5
    done < "$WORK/variants.cur"
    r=$((r + 1))
done

if command -v python3 >/dev/null 2>&1; then
    python3 "$REPO/tools/perf/alloc_ab_summary.py" "$OUT"
else
    log "python3 が無いので集計は host 側で: python3 tools/perf/alloc_ab_summary.py $OUT"
fi
