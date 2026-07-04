#!/bin/bash
# Veil パフォーマンス計測ハーネス（glibc / musl / nginx をコンテナ間通信で比較）
# 使い方: リポジトリのどこからでも  bash tools/perf/run_perf.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"          # -> tools/perf
REPO_ROOT="$(cd "$HERE/../.." && pwd)"          # -> リポジトリルート
ASSETS="$REPO_ROOT/docker/assets"               # ssl / www / security/seccomp.json の所在
NET=perf_net
RESULTS="$HERE/results/results.tsv"
LOGDIR="$HERE/results/logs"
mkdir -p "$LOGDIR"

WRK_IMG=williamyeh/wrk:latest
H2_IMG=local/h2load:latest

# 負荷パラメータ（環境変数で上書き可）
WRK_ARGS="${WRK_ARGS:--t4 -c100 -d10s --timeout 5s --latency}"
H2_ARGS="${H2_ARGS:--n 30000 -c100 -m10}"
ITERATIONS="${ITERATIONS:-3}"      # 各 (config, proto) の反復回数（median±stdev 集計用）

# 計測結果は raw（1 反復 1 行）で保存し、analyze_results.sh で median±stdev に集計する。
RESULTS_RAW="$HERE/results/results_raw.tsv"
SUMMARY="$HERE/results/results_summary.md"

docker network inspect $NET >/dev/null 2>&1 || docker network create $NET >/dev/null

echo -e "target\tconfig\tproto\titeration\treq_per_sec\ttransfer\tlat_avg\tlat_p99\tnon2xx\tcpu_pct\tmem_mb" > "$RESULTS_RAW"

emit() { # target config proto iteration reqps transfer latavg latp99 non2xx cpu mem
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$@" >> "$RESULTS_RAW"
}

sample_stats() { # container -> "cpu_pct mem_mb" averaged over 3 samples during load
    local c="$1" i cpu mem csum=0 msum=0 n=0
    for i in 1 2 3; do
        read cpu mem < <(docker stats --no-stream --format '{{.CPUPerc}} {{.MemUsage}}' "$c" 2>/dev/null \
            | awk '{gsub(/%/,"",$1); m=$2; gsub(/MiB/,"",m); gsub(/GiB/,"",m); print $1, m}')
        [ -z "${cpu:-}" ] && continue
        csum=$(awk -v a="$csum" -v b="$cpu" 'BEGIN{print a+b}')
        msum=$(awk -v a="$msum" -v b="$mem" 'BEGIN{print a+b}')
        n=$((n+1))
    done
    [ "$n" -eq 0 ] && { echo "NA NA"; return; }
    awk -v c="$csum" -v m="$msum" -v n="$n" 'BEGIN{printf "%.1f %.1f\n", c/n, m/n}'
}

parse_wrk() { # logfile -> "reqps transfer latavg latp99 non2xx"
    local f="$1"
    local reqps transfer latavg latp99 non2xx
    reqps=$(awk '/Requests\/sec:/{print $2}' "$f")
    transfer=$(awk '/Transfer\/sec:/{print $2}' "$f")
    latavg=$(awk '/Latency/{print $2; exit}' "$f")           # from Thread Stats line
    latp99=$(awk '/ 99%/{print $2}' "$f")                    # from --latency distribution
    non2xx=$(awk '/Non-2xx or 3xx/{print $NF}' "$f"); [ -z "$non2xx" ] && non2xx=0
    echo "${reqps:-NA} ${transfer:-NA} ${latavg:-NA} ${latp99:-NA} ${non2xx:-0}"
}

parse_h2load() { # logfile -> "reqps throughput latmean non2xx"
    local f="$1"
    # finished in 3.20s, 9375.00 req/s, 5.71GB/s  のような行
    local reqps tput non2xx latmean
    reqps=$(awk -F', ' '/finished in/{for(i=1;i<=NF;i++) if($i ~ /req\/s/){gsub(/ req\/s/,"",$i); print $i}}' "$f")
    tput=$(awk -F', ' '/finished in/{print $3}' "$f")
    # status codes: "status codes: 30000 2xx, 0 3xx, 0 4xx, 0 5xx"
    local twoxx err4 err5
    twoxx=$(awk -F'[ ,]+' '/status codes:/{for(i=1;i<=NF;i++) if($(i+1)=="2xx") print $i}' "$f")
    err4=$(awk -F'[ ,]+' '/status codes:/{for(i=1;i<=NF;i++) if($(i+1)=="4xx") print $i}' "$f")
    err5=$(awk -F'[ ,]+' '/status codes:/{for(i=1;i<=NF;i++) if($(i+1)=="5xx") print $i}' "$f")
    non2xx=$(( ${err4:-0} + ${err5:-0} ))
    latmean=$(awk '/time for request:/{print $6}' "$f")   # mean（min,max,mean の3列目）
    echo "${reqps:-NA} ${tput:-NA} ${latmean:-NA} ${non2xx:-0}"
}

run_load() { # target_label config_label container has_http2
    local label="$1" cfg="$2" c="$3" h2="$4" iter reqps transfer latavg latp99 non2xx cpu mem tput latmean

    # ウォームアップ（JIT/ページキャッシュ/接続確立コストを計測外にする）
    docker run --rm --network $NET $WRK_IMG -t2 -c10 -d2s "https://$c:443/" >/dev/null 2>&1

    # HTTP/1.1 (wrk) × ITERATIONS
    for iter in $(seq 1 "$ITERATIONS"); do
        ( sleep 2; sample_stats "$c" > "$LOGDIR/${label}_${cfg}_wrk_${iter}.stats" ) &
        docker run --rm --network $NET $WRK_IMG $WRK_ARGS "https://$c:443/" \
            > "$LOGDIR/${label}_${cfg}_wrk_${iter}.log" 2>&1
        wait
        read reqps transfer latavg latp99 non2xx < <(parse_wrk "$LOGDIR/${label}_${cfg}_wrk_${iter}.log")
        read cpu mem < "$LOGDIR/${label}_${cfg}_wrk_${iter}.stats" 2>/dev/null || { cpu=NA; mem=NA; }
        emit "$label" "$cfg" "http1.1" "$iter" "$reqps" "$transfer" "$latavg" "$latp99" "$non2xx" "$cpu" "$mem"
    done

    # HTTP/2 (h2load) × ITERATIONS
    if [ "$h2" = "1" ]; then
        docker run --rm --network $NET --entrypoint h2load $H2_IMG -n 1000 -c 10 "https://$c:443/" >/dev/null 2>&1
        for iter in $(seq 1 "$ITERATIONS"); do
            ( sleep 1; sample_stats "$c" > "$LOGDIR/${label}_${cfg}_h2_${iter}.stats" ) &
            docker run --rm --network $NET --entrypoint h2load $H2_IMG $H2_ARGS "https://$c:443/" \
                > "$LOGDIR/${label}_${cfg}_h2_${iter}.log" 2>&1
            wait
            read reqps tput latmean non2xx < <(parse_h2load "$LOGDIR/${label}_${cfg}_h2_${iter}.log")
            read cpu mem < "$LOGDIR/${label}_${cfg}_h2_${iter}.stats" 2>/dev/null || { cpu=NA; mem=NA; }
            emit "$label" "$cfg" "http2" "$iter" "$reqps" "$tput" "$latmean" "NA" "$non2xx" "$cpu" "$mem"
        done
    fi
}

start_veil() { # image config_file container_name
    local img="$1" cfgfile="$2" name="$3"
    docker rm -f "$name" >/dev/null 2>&1
    docker run -d --rm --network $NET \
        --read-only \
        --tmpfs /var/cache/veil:rw,noexec,nosuid,uid=65532,gid=65532,size=512m \
        --tmpfs /var/tmp/veil:rw,noexec,nosuid,uid=65532,gid=65532,size=256m \
        -v "$cfgfile:/etc/veil/conf.d/config.toml:ro" \
        -v "$ASSETS/ssl:/etc/veil/ssl:ro" \
        -v "$ASSETS/www:/var/www:ro" \
        --security-opt seccomp="$ASSETS/security/seccomp.json" \
        --name "$name" "$img" >/dev/null
}

wait_ready() { # container
    local c="$1" i
    for i in $(seq 1 20); do
        if docker run --rm --network $NET curlimages/curl:latest -sk -o /dev/null \
             -w '%{http_code}' "https://$c:443/" 2>/dev/null | grep -q 200; then
            return 0
        fi
        sleep 0.5
    done
    echo "!! $c not ready" >&2
    docker logs "$c" 2>&1 | tail -8 >&2
    return 1
}

# ---- nginx baseline ----
echo "### nginx"
docker rm -f nginx-perf >/dev/null 2>&1
docker run -d --rm --network $NET \
    -v "$HERE/nginx/nginx.conf:/etc/nginx/nginx.conf:ro" \
    -v "$ASSETS/ssl:/etc/veil/ssl:ro" \
    -v "$ASSETS/www:/var/www:ro" \
    --name nginx-perf nginx:alpine >/dev/null
if wait_ready nginx-perf; then
    run_load nginx base nginx-perf 1
fi
docker rm -f nginx-perf >/dev/null 2>&1

# ---- veil glibc / musl x configs ----
for build in glibc musl; do
    img="veil:$build"
    for cfgfile in "$HERE"/configs/*.toml; do
        name=$(basename "$cfgfile" .toml)
        # 名前 h2_1_* は HTTP/2 有効 → h2load も実施。h2_0_* は HTTP/1.1 のみ。
        case "$name" in h2_1_*) h2=1 ;; *) h2=0 ;; esac
        echo "### veil:$build / $name"
        start_veil "$img" "$cfgfile" veil-container
        if wait_ready veil-container; then
            run_load "veil_$build" "$name" veil-container "$h2"
        else
            emit "veil_$build" "$name" "http1.1" 1 NA NA NA NA NA NA NA
        fi
        docker rm -f veil-container >/dev/null 2>&1
    done
done

echo "=== 生データ: $RESULTS_RAW（${ITERATIONS} 反復/構成）==="
# median±stdev へ集計してサマリ Markdown を生成
if bash "$HERE/analyze_results.sh" "$RESULTS_RAW" > "$SUMMARY" 2>/dev/null; then
    echo "=== 集計: $SUMMARY ==="
    cat "$SUMMARY"
else
    echo "!! 集計に失敗（生データは $RESULTS_RAW 参照）" >&2
fi
