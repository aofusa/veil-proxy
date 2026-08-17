#!/bin/sh
# =============================================================================
# run_perf_freebsd.sh — FreeBSD ネイティブ perf 計測ハーネス（veil vs nginx）
# =============================================================================
#
# tools/perf/ の本体は Docker 前提（glibc/musl イメージ間のコンテナ間通信）だが、
# FreeBSD には Docker が無いため **ゲスト内 loopback** で veil と nginx を同条件で
# 突き合わせる専用ハーネスを用意する（F-145）。
#
# 実行場所 : FreeBSD ゲスト内・root
# 前提     : pkg install nginx nghttp2 wrk-luajit curl
#            （nginx は --with-http_v2_module / --with-http_v3_module / --with-stream 付き）
#
# HTTP/3 計測（h3_file シナリオ）:
#   FreeBSD の nghttp2 pkg の h2load は ngtcp2 非搭載で QUIC を計測できない（curl pkg も
#   HTTP/3 非対応）。そのため quinn + h3（テスト・計測ツール向けの HTTP/3 ライブラリ。
#   本番データプレーンの quiche とは別方針、AGENTS.md 参照）で作った自前クライアント
#   `tools/perf/h3load`（`${REPO}/tools/perf/h3load/target/release/h3load`）を優先して使う。
#   事前にビルドしておくこと（FreeBSD の perf ビルドは
#   h3load は **veil 本体のワークスペース外の独立クレート**なので、本体の feature とは
#   無関係に単体でビルドする（本体の通常ビルド・パッケージングには一切含まれない）:
#     cargo build --release --manifest-path tools/perf/h3load/Cargo.toml
#   （quinn + h3 は cmake 不要でビルドできる）
#   ビルド済みバイナリが無い場合は `h2load --h3` にフォールバックする（QUIC 非対応ビルドでは
#   計測失敗になる点に注意。ログに警告を出す）。
#
# 公平化のためのルール:
#   - veil / nginx とも **同じ 2 コア**（cpuset 0,1）に固定し、負荷生成側は
#     残り 2 コア（cpuset 2,3）に固定する。計測対象と負荷生成の CPU 競合を排除する。
#   - アクセスログは双方オフ（veil は logging.level=warn、nginx は access_log off）。
#   - 静的配信の上流・配信ファイルは同一のものを使う。
#   - proxy 構成の上流は **どちらの計測でも同じ nginx**（:18080）を使う。
#
# 使い方:
#   sh tools/perf/freebsd/run_perf_freebsd.sh [-r 回数] [-d 秒] [-o 出力TSV] [シナリオ...]
#   （シナリオ省略時は全件）
#
#   例) sh tools/perf/freebsd/run_perf_freebsd.sh -r 3 -d 15
#       sh tools/perf/freebsd/run_perf_freebsd.sh h1_file_tls h2_file_tls
#
# 計測結果の記録先: docs/perf/README.md の「FreeBSD ネイティブ計測」節（分析）と
#   docs/perf/freebsd_results_raw.tsv（生データ）。公開する結果はこの 2 つへ反映すること。
#
# 出力: TSV（既定 ${WORK}/results_raw.tsv）
#   scenario  server  iter  rps  transfer_mbps  p50_ms  p99_ms  errors
# =============================================================================

set -eu

# ---------------------------------------------------------------------------
# 設定
# ---------------------------------------------------------------------------
WORK="${WORK:-/tmp/veilperf}"
REPO="${REPO:-$(cd "$(dirname "$0")/../../.." && pwd)}"
VEIL_BIN="${VEIL_BIN:-${REPO}/target/release/veil}"
# HTTP/3 (QUIC) 負荷生成: quinn + h3 ベースの自前クライアント（tools/perf/h3load、独立クレート）。
# 無ければ measure() が h2load --h3 へフォールバックする（QUIC 非対応ビルドでは失敗する）。
H3LOAD_BIN="${H3LOAD_BIN:-${REPO}/tools/perf/h3load/target/release/h3load}"

ITERATIONS="${ITERATIONS:-3}"
DURATION="${DURATION:-15}"
CONNECTIONS="${CONNECTIONS:-64}"
LOAD_THREADS="${LOAD_THREADS:-2}"
# 計測対象を固定するコア / 負荷生成を固定するコア
SRV_CPUS="${SRV_CPUS:-0,1}"
GEN_CPUS="${GEN_CPUS:-2,3}"
SRV_THREADS="${SRV_THREADS:-2}"

# ポート割り当て（veil / nginx / 共通上流）
VEIL_HTTP=4080
VEIL_HTTPS=4443
VEIL_L4=4090
NGX_HTTP=5080
NGX_HTTPS=5443
NGX_L4=5090
UPSTREAM=18080

OUT_TSV="${OUT_TSV:-${WORK}/results_raw.tsv}"
# 計測対象パス（-p で切替）。小さいファイル（/small.html）にするとバイト単価ではなく
# リクエスト単価（syscall・パース・フレーミング）の比較になる。
REQ_PATH="${REQ_PATH:-/index.html}"

ALL_SCENARIOS="h1_file_tls h2_file_tls h1_file_plain h2c_file_plain h3_file h1_proxy_tls h2_proxy_tls l4_tcp"

usage() {
    sed -n '2,32p' "$0"
    exit 1
}

while getopts "r:d:c:o:p:h" opt; do
    case "$opt" in
        r) ITERATIONS="$OPTARG" ;;
        d) DURATION="$OPTARG" ;;
        c) CONNECTIONS="$OPTARG" ;;
        o) OUT_TSV="$OPTARG" ;;
        p) REQ_PATH="$OPTARG" ;;
        h|*) usage ;;
    esac
done
shift $((OPTIND - 1))
SCENARIOS="${*:-$ALL_SCENARIOS}"

log() { printf '[perf] %s\n' "$*" >&2; }
die() { printf '[perf] ERROR: %s\n' "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# 準備
# ---------------------------------------------------------------------------
prepare() {
    mkdir -p "${WORK}/www" "${WORK}/ssl" "${WORK}/logs" "${WORK}/conf" "${WORK}/cache" "${WORK}/nginx/logs"

    # FreeBSD の kTLS は既定で無効（kern.ipc.tls.enable=0）。カーネル側の能力自体は
    # 有効化しておく（kTLS を明示的に測りたいバリエーションのため。GENERIC カーネルは
    # ktls_ocf を内蔵しているので kldload は不要）。
    # ただし **veil の既定計測は ktls_enabled = false** にしてある（_veil_common_head）:
    # FreeBSD の software kTLS は TLS レコード（16KB）ごとにカーネルワーカースレッドへ
    # 暗号処理をディスパッチするため、コンテキストスイッチが秒間 40 万回に達して
    # 帯域が 1.2 GB/s で直列化する（docs/perf/README.md の FreeBSD 節）。nginx 側も
    # kTLS を使わない構成（ssl_conf_command 未指定）なので、無効側が公平な比較になる。
    sysctl kern.ipc.tls.enable=1 >/dev/null 2>&1 || log "kTLS を有効化できなかった（計測は継続）"
    # listen backlog の既定 128 は 64 コネクションの負荷でも accept キュー溢れを起こしうる。
    # veil / nginx 双方に等しく効くので引き上げる。
    sysctl kern.ipc.somaxconn=4096 >/dev/null 2>&1 || true

    # 配信コンテンツ: 大（既存 perf と同じ 54KB）と小（100B）の 2 種
    # REQ_PATH に関わらず両方を用意する（-p /index.html ⇔ -p /small.html を切り替えて
    # 「バイト単価」と「リクエスト単価」を別々に見るため）。
    if [ -f "${REPO}/docker/assets/www/index.html" ]; then
        cp "${REPO}/docker/assets/www/index.html" "${WORK}/www/index.html"
    else
        : > "${WORK}/www/index.html"
        i=0; while [ $i -lt 1000 ]; do printf '<p>veil perf filler line %04d</p>\n' "$i" >> "${WORK}/www/index.html"; i=$((i+1)); done
    fi
    printf 'ok\n' > "${WORK}/www/small.html"

    # 自己署名証明書
    if [ ! -f "${WORK}/ssl/cert.pem" ]; then
        log "自己署名証明書を生成"
        openssl req -x509 -newkey rsa:2048 -nodes -days 365 \
            -keyout "${WORK}/ssl/key.pem" -out "${WORK}/ssl/cert.pem" \
            -subj "/CN=localhost" -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" \
            >/dev/null 2>&1 || die "証明書生成に失敗"
    fi

    [ -x "${VEIL_BIN}" ] || die "veil バイナリが見つからない: ${VEIL_BIN}"
    command -v nginx  >/dev/null || die "nginx が無い（pkg install nginx）"
    command -v wrk    >/dev/null || die "wrk が無い（pkg install wrk-luajit）"
    command -v h2load >/dev/null || die "h2load が無い（pkg install nghttp2）"

    write_nginx_conf
    write_veil_configs
}

# ---------------------------------------------------------------------------
# nginx 設定（上流 + 計測対象の両方を 1 プロセスで賄う）
# ---------------------------------------------------------------------------
write_nginx_conf() {
    # FreeBSD の公式 pkg は stream を **動的モジュール**（--with-stream=dynamic）で
    # ビルドしているため、L4 計測に使う `stream {}` は load_module が必要。
    stream_mod=""
    for so in /usr/local/libexec/nginx/ngx_stream_module.so; do
        [ -f "$so" ] && stream_mod="load_module $so;"
    done

    cat > "${WORK}/conf/nginx.conf" <<EOF
${stream_mod}
worker_processes ${SRV_THREADS};
daemon on;
pid ${WORK}/nginx/nginx.pid;
error_log ${WORK}/logs/nginx_error.log warn;
events { worker_connections 8192; }

http {
    default_type application/octet-stream;
    access_log off;
    sendfile on;
    tcp_nopush on;
    tcp_nodelay on;
    keepalive_timeout 65;
    keepalive_requests 1000000;

    # 静的配信の推奨チューニング。veil 側で静的コンテンツキャッシュ（F-146）を
    # 有効にしているため、nginx にも同等の open_file_cache を入れて条件を揃える
    # （open/fstat/close をリクエストごとに繰り返さない状態どうしで比較する）。
    open_file_cache max=1024 inactive=60s;
    open_file_cache_valid 60s;
    open_file_cache_min_uses 1;
    open_file_cache_errors on;

    # ---- 共通上流（veil / nginx の proxy・L4 計測の双方が使う）----
    server {
        listen 127.0.0.1:${UPSTREAM} reuseport;
        root ${WORK}/www;
        location / { index index.html; }
    }

    upstream backend { server 127.0.0.1:${UPSTREAM}; keepalive 128; }

    # ---- 計測対象 nginx: 平文 HTTP/1.1（静的 + プロキシ）----
    server {
        listen ${NGX_HTTP} reuseport;
        # h2c（平文 HTTP/2 prior knowledge）を受け付ける。veil 側の平文リスナーが
        # h2c 専用なので、比較対象の nginx も h2c を有効にしないと条件が揃わない。
        http2 on;
        root ${WORK}/www;
        location / { index index.html; }
        location /proxy/ {
            proxy_pass http://backend/;
            proxy_http_version 1.1;
            proxy_set_header Connection "";
        }
    }

    # ---- 計測対象 nginx: TLS HTTP/1.1 + HTTP/2 + HTTP/3 ----
    server {
        listen ${NGX_HTTPS} ssl reuseport;
        listen ${NGX_HTTPS} quic reuseport;
        http2 on;
        http3 on;
        ssl_certificate     ${WORK}/ssl/cert.pem;
        ssl_certificate_key ${WORK}/ssl/key.pem;
        ssl_protocols TLSv1.2 TLSv1.3;
        ssl_session_cache shared:SSL:32m;
        root ${WORK}/www;
        location / { index index.html; }
        location /proxy/ {
            proxy_pass http://backend/;
            proxy_http_version 1.1;
            proxy_set_header Connection "";
        }
    }
}

stream {
    upstream l4backend { server 127.0.0.1:${UPSTREAM}; }
    server {
        listen ${NGX_L4} reuseport;
        proxy_pass l4backend;
    }
}
EOF
}

# ---------------------------------------------------------------------------
# veil 設定（シナリオ別）
# ---------------------------------------------------------------------------
# $1 に [server] セクションへ追記する行を渡せる（省略可）
_veil_common_head() {
    cat <<EOF
# 自動生成: FreeBSD perf 計測用（run_perf_freebsd.sh）
[server]
listen = "0.0.0.0:${VEIL_HTTPS}"
http2_enabled = true
threads = ${SRV_THREADS}
# 平文 HTTP/1.1・h2c の計測用リスナー。
# [server].http は「HTTP→HTTPS 301 リダイレクト専用」なので平文の実配信には使えない
# （指定すると平文計測が 301 の空応答を測ることになる）。h2c リスナーを使う。
h2c_enabled = true
h2c_listen = "0.0.0.0:${VEIL_HTTP}"
${1:-}

[logging]
level = "warn"

# ベンチマークは **可能な限りセキュリティ機能を有効にした状態**で測る
# （本番に近い条件で測らなければ意味がないため。2026-08-16 に有効化）。
#   - capsicum + capability mode: FreeBSD の主要なサンドボックス。F-123 で
#     capability mode 下の静的配信（dirfd + openat/O_RESOLVE_BENEATH）が
#     動作するようにしてあるので、この構成は F-123/F-153 の回帰テストも兼ねる。
#   - allow_security_failures = false: セキュリティ機能の適用に失敗したら
#     黙って無効化せず起動を止める（「有効にしたつもり」を防ぐ）。
[security]
allow_security_failures = false
enable_capsicum = true
capsicum_capability_mode = true

[tls]
cert_path = "${WORK}/ssl/cert.pem"
key_path = "${WORK}/ssl/key.pem"
ktls_enabled = false
ktls_fallback_enabled = true

# 静的コンテンツキャッシュ（F-146）を有効化する。
#
# HTTP/2・HTTP/3 は DATA フレーム / QUIC ストリームへの再フレーミングが要るため
# `sendfile(2)` に載せられず、ファイル本体をユーザ空間へ読み出す必要がある。
# 無効のままだと **1 リクエストごとに** offload スレッドプールへ往復して
# open/fstat/lseek/read/close を実行する（F-157 の DTrace 実測で 1 リクエストあたり
# openat 1.0 / fstat 2.0 / lseek 1.0 / close 1.0 / read 3.0 / 完了通知パイプの
# 1 バイト write 1.0 / _umtx_op 1.6 を確認）。これが h2c 平文で nginx に劣後する
# 主因だった。
#
# 比較対象の nginx 側も `open_file_cache` を有効にしてあり（生成する nginx.conf を
# 参照）、**双方とも「静的配信向けに推奨設定を入れた状態」で突き合わせる**。
# 片方だけチューニングして測ってはならない（F-155 の kTLS と同じ失敗）。
[static_file_cache]
enabled = true
valid_duration_secs = 60
max_entries = 1024
max_file_size_bytes = 1048576
EOF
}

_veil_route_file() {
    cat <<EOF

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "${WORK}/www/"
[route.security]
allowed_methods = ["HEAD", "GET"]

# メタデータキャッシュ（open_file_cache）。**[static_file_cache] と必ずセットで
# 有効にすること**（F-157）: `cache::get_static_file_with_content` の offload ゼロ
# 経路は「メタデータキャッシュがヒットしたときに限り本体キャッシュを参照する」
# 構造（src/cache/static_file.rs）なので、本体キャッシュだけ有効にしても
# 素通りして毎リクエスト offload の open+read に落ちる（実測で openat が
# 1.0/req のまま変わらず、スループットも改善しなかった）。
[route.open_file_cache]
enabled = true
valid_duration_secs = 60
max_entries = 1024
EOF
}

_veil_route_proxy() {
    cat <<EOF

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:${UPSTREAM}/"
[route.security]
allowed_methods = ["HEAD", "GET"]
EOF
}

write_veil_configs() {
    # 静的配信
    { _veil_common_head; _veil_route_file; } > "${WORK}/conf/veil_file.toml"

    # 静的配信 + HTTP/3（有効化は [server].http3_enabled、UDP リッスンは [http3].listen）
    { _veil_common_head 'http3_enabled = true'
      cat <<EOF

[http3]
listen = "0.0.0.0:${VEIL_HTTPS}"
EOF
      _veil_route_file
    } > "${WORK}/conf/veil_file_h3.toml"

    # プロキシ
    { _veil_common_head; _veil_route_proxy; } > "${WORK}/conf/veil_proxy.toml"

    # L4 TCP
    { _veil_common_head
      _veil_route_file
      cat <<EOF

[[l4]]
name = "tcp_perf"
listen = "0.0.0.0:${VEIL_L4}"
protocol = "tcp"
tls = "none"
  [[l4.upstreams]]
  addr = "127.0.0.1:${UPSTREAM}"
EOF
    } > "${WORK}/conf/veil_l4.toml"
}

# ---------------------------------------------------------------------------
# サーバのライフサイクル
# ---------------------------------------------------------------------------

# 計測対象 veil を確実に停止する。
#
# **`pkill -x veil` だけでは不十分**（実測で踏んだ）: `-x` はプロセス名の**完全一致**
# なので、`VEIL_BIN` に `veil.f156` のような別名のバイナリを指定して A/B すると
# 1 つも kill されない。さらに veil のリスナーは `SO_REUSEPORT`(_LB) で bind するため、
# **取り残された旧プロセスが同じポートを掴んだまま生き残り、カーネルが新旧プロセスへ
# 接続を分散してしまう**（＝別バイナリの混合を計測する）。実際にこれで A/B が丸ごと
# 無効化され、取り残しが 13 プロセスまで積み上がって「ラウンドごとに単調劣化する」
# という誤った結論を出しかけた。
#
# そのため (1) 実行中の `VEIL_BIN` をフルパスで狙い撃ちし、(2) 名前が `veil` の
# プロセスも従来どおり落とす、の 2 段構えにする。
stop_veil() {
    pkill -f "^${VEIL_BIN}( |$)" >/dev/null 2>&1 || true
    pkill -x veil >/dev/null 2>&1 || true
}

stop_all() {
    stop_veil
    if [ -f "${WORK}/nginx/nginx.pid" ]; then
        nginx -c "${WORK}/conf/nginx.conf" -s quit >/dev/null 2>&1 || true
    fi
    pkill -x nginx >/dev/null 2>&1 || true
    sleep 1
}

start_nginx() {
    nginx -c "${WORK}/conf/nginx.conf" >/dev/null 2>&1 \
        || die "nginx 起動に失敗（${WORK}/logs/nginx_error.log）"
    wait_port "${UPSTREAM}" || die "nginx 上流が上がらない"
}

start_veil() {
    cfg="$1"; shift
    # 起動前に veil のプロセスが 1 つも残っていないことを確認する。
    # SO_REUSEPORT(_LB) のため、取り残しがいると新旧プロセスが同じポートを共有し、
    # カーネルが両方へ接続を分散して**別バイナリの混合を計測してしまう**
    # （`stop_veil` のコメント参照。実際に A/B を丸ごと無効化した）。
    # 残っていたら黙って続行せず、計測を止める。
    leftover=$(pgrep veil | wc -l | tr -d ' ')
    if [ "${leftover}" != "0" ]; then
        pgrep -l veil >&2 || true
        die "veil のプロセスが ${leftover} 個残っている（計測が混ざるので中止）"
    fi
    # cpuset で計測対象コアへ固定する。veil 自身の CPU アフィニティ設定は
    # cpuset のマスク内で解決されるため二重指定でも問題ない。
    cpuset -l "${SRV_CPUS}" "${VEIL_BIN}" -c "${cfg}" "$@" \
        > "${WORK}/logs/veil.log" 2>&1 &
    echo $! > "${WORK}/veil.pid"
}

wait_port() {
    p="$1"; n=0
    while [ $n -lt 100 ]; do
        if sockstat -4 -l -p "$p" 2>/dev/null | grep -q ":$p"; then return 0; fi
        sleep 0.2; n=$((n+1))
    done
    return 1
}

wait_udp_port() {
    p="$1"; n=0
    while [ $n -lt 100 ]; do
        if sockstat -4 -l -u -p "$p" 2>/dev/null | grep -q ":$p"; then return 0; fi
        sleep 0.2; n=$((n+1))
    done
    return 1
}

# ---------------------------------------------------------------------------
# 負荷生成
# ---------------------------------------------------------------------------
# run_wrk <url> -> "rps<TAB>mbps<TAB>p50<TAB>p99<TAB>errors"
run_wrk() {
    url="$1"
    out=$(timeout $((DURATION * 4 + 60)) cpuset -l "${GEN_CPUS}" wrk -t"${LOAD_THREADS}" -c"${CONNECTIONS}" \
            -d"${DURATION}s" --latency --timeout 10s "$url" 2>&1) || true
    printf '%s\n' "$out" >> "${WORK}/logs/wrk.log"
    printf '%s' "$out" | awk '
        /^Requests\/sec:/ { rps=$2 }
        /^Transfer\/sec:/ { t=$2;
            if (t ~ /GB$/)      { sub(/GB$/,"",t); mbps=t*8*1024 }
            else if (t ~ /MB$/) { sub(/MB$/,"",t); mbps=t*8 }
            else if (t ~ /KB$/) { sub(/KB$/,"",t); mbps=t*8/1024 }
            else                { sub(/B$/,"",t);  mbps=t*8/1024/1024 } }
        /^ +50%/ { p50=lat($2) }
        /^ +99%/ { p99=lat($2) }
        /Socket errors/ { err=$0; gsub(/[^0-9 ]/," ",err); n=split(err,a," "); e=0; for(i=1;i<=n;i++) e+=a[i] }
        function lat(v) {
            if (v ~ /us$/) { sub(/us$/,"",v); return v/1000 }
            if (v ~ /ms$/) { sub(/ms$/,"",v); return v+0 }
            if (v ~ /s$/)  { sub(/s$/,"",v);  return v*1000 }
            return v+0 }
        END { printf "%s\t%.2f\t%.3f\t%.3f\t%d", (rps==""?0:rps), mbps, p50, p99, (e==""?0:e) }'
}

# run_h2load <url> [extra args] -> 同上
run_h2load() {
    url="$1"; shift
    total=$(( CONNECTIONS * 2000 ))
    out=$(timeout $((DURATION * 4 + 60)) cpuset -l "${GEN_CPUS}" h2load -t"${LOAD_THREADS}" -c"${CONNECTIONS}" \
            -m 32 -n "${total}" "$@" "$url" 2>&1) || true
    printf '%s\n' "$out" >> "${WORK}/logs/h2load.log"
    printf '%s' "$out" | awk '
        /req\/s/ && /finished in/ { for(i=1;i<=NF;i++) if($i=="req/s,") rps=$(i-1) }
        /finished in/ { for(i=1;i<=NF;i++) if($i ~ /B\/s,?$/) { v=$i; sub(/,$/,"",v);
            if (v ~ /GB\/s$/)      { sub(/GB\/s$/,"",v); mbps=v*8*1024 }
            else if (v ~ /MB\/s$/) { sub(/MB\/s$/,"",v); mbps=v*8 }
            else if (v ~ /KB\/s$/) { sub(/KB\/s$/,"",v); mbps=v*8/1024 }
            else                   { sub(/B\/s$/,"",v);  mbps=v*8/1024/1024 } } }
        /^time for request:/ { p50=lat($5) }
        /^ *status codes:/ { }
        /requests:/ && /failed/ { for(i=1;i<=NF;i++) if($i=="failed,") e=$(i-1) }
        function lat(v) {
            if (v ~ /us$/) { sub(/us$/,"",v); return v/1000 }
            if (v ~ /ms$/) { sub(/ms$/,"",v); return v+0 }
            if (v ~ /s$/)  { sub(/s$/,"",v);  return v*1000 }
            return v+0 }
        END { printf "%s\t%.2f\t%.3f\t%.3f\t%d", (rps==""?0:rps), mbps, p50, 0, (e==""?0:e) }'
}

# run_h3load <url> -> 同上（h3load.rs の固定書式を前提にパースする。h2load と違い
# こちらは同梱の tools/perf/h3load が出力する書式なので構造が完全に既知）。
#
# **`-n`（固定リクエスト数）ではなく `-d`（実行時間）を使うこと**（2026-08-11 修正）:
# 以前は `-n $((CONNECTIONS * 2000))`（既定 64 接続なら 128,000 リクエスト）を
# 外側の `timeout $((DURATION * 4 + 60))` で包んでいたため、**サーバが遅い環境では
# 指定本数を捌き切る前に timeout に殺され、h3load が集計行を出力しないまま
# 0 rps として記録される**（しかもエラーにならないので気付けない）。実際
# FreeBSD amd64 (QEMU/KVM) では h3_file が veil・nginx とも 0 rps になっていた。
# wrk と同じ「時間で区切る」方式にすれば、マシンの速度に依存せず必ず集計行が出る。
run_h3load() {
    url="$1"
    out=$(timeout $((DURATION * 4 + 60)) cpuset -l "${GEN_CPUS}" "${H3LOAD_BIN}" \
            -t"${LOAD_THREADS}" -c"${CONNECTIONS}" -m 32 -d "${DURATION}" "$url" 2>&1) || true
    printf '%s\n' "$out" >> "${WORK}/logs/h3load.log"
    printf '%s' "$out" | awk '
        /^finished in/ {
            rps = $4
            bps = $6; sub(/B\/s,?$/,"",bps); mbps = bps*8/1024/1024
        }
        /^requests:/ {
            failed=0; errored=0
            for (i=1;i<=NF;i++) {
                if ($i=="failed,")  failed=$(i-1)
                if ($i=="errored,") errored=$(i-1)
            }
            e = failed + errored
        }
        /^time for request:/ { p50=lat($11); p99=lat($13) }
        function lat(v) {
            if (v ~ /us$/) { sub(/us$/,"",v); return v/1000 }
            if (v ~ /ms$/) { sub(/ms$/,"",v); return v+0 }
            if (v ~ /s$/)  { sub(/s$/,"",v);  return v*1000 }
            return v+0 }
        END { printf "%s\t%.2f\t%.3f\t%.3f\t%d", (rps==""?0:rps), mbps, p50, p99, (e==""?0:e) }'
}

# ---------------------------------------------------------------------------
# シナリオ実行
# ---------------------------------------------------------------------------
# 各シナリオは「veil の config」「veil の URL」「nginx の URL」「負荷ツール」を決める
run_scenario() {
    sc="$1"; iter="$2"

    case "$sc" in
      h1_file_tls)
        v_cfg=veil_file.toml;    v_url="https://127.0.0.1:${VEIL_HTTPS}${REQ_PATH}"
        n_url="https://127.0.0.1:${NGX_HTTPS}${REQ_PATH}"; tool=wrk ;;
      h2_file_tls)
        v_cfg=veil_file.toml;    v_url="https://127.0.0.1:${VEIL_HTTPS}${REQ_PATH}"
        n_url="https://127.0.0.1:${NGX_HTTPS}${REQ_PATH}"; tool=h2load ;;
      h2c_file_plain)
        v_cfg=veil_file.toml;    v_url="http://127.0.0.1:${VEIL_HTTP}${REQ_PATH}"
        n_url="http://127.0.0.1:${NGX_HTTP}${REQ_PATH}"; tool=h2load ;;
      # F-155: 平文 HTTP/1.1 の静的配信。veil の FreeBSD `sendfile(2)` ゼロコピー経路
      # （`ServerTls::is_plain()` = TLS 終端なしのときだけ通る）が実際に効く唯一の
      # シナリオであり、sf_hdtr による 1-syscall 化の効果はここでしか観測できない
      # （h2c は HTTP/2 のフレーミングが要るので sendfile に載らない）。
      # nginx 側も `sendfile on;` で同じくカーネルゼロコピー + sf_hdtr を使うため、
      # 「1 リクエストあたりの syscall 数」を真正面から比較する構成になる。
      #
      # **veil の宛先が HTTPS ポートなのは誤記ではない。** veil の平文リスナー
      # （`h2c_listen`）は **h2c 専用**で、平文 HTTP/1.1 は
      # 「Plain HTTP/1.1 not supported on H2C-only server」として切断される。
      # 一方、メインリスナーは先頭バイトのプロトコル検出（`detect_protocol_with_buffer`）
      # を行い、TLS ClientHello でなければ平文 HTTP/1.1 として `accept_plain` する。
      # したがって「veil に平文 HTTP/1.1 を喋らせる」唯一の方法は、TLS ポートへ
      # `http://` で投げることである（実機で 200 / Content-Length 一致 / 本体 md5 一致を確認済み）。
      h1_file_plain)
        v_cfg=veil_file.toml;    v_url="http://127.0.0.1:${VEIL_HTTPS}${REQ_PATH}"
        n_url="http://127.0.0.1:${NGX_HTTP}${REQ_PATH}"; tool=wrk ;;
      h3_file)
        v_cfg=veil_file_h3.toml; v_url="https://127.0.0.1:${VEIL_HTTPS}${REQ_PATH}"
        n_url="https://127.0.0.1:${NGX_HTTPS}${REQ_PATH}"; tool=h2load_h3 ;;
      h1_proxy_tls)
        v_cfg=veil_proxy.toml;   v_url="https://127.0.0.1:${VEIL_HTTPS}${REQ_PATH}"
        n_url="https://127.0.0.1:${NGX_HTTPS}/proxy${REQ_PATH}"; tool=wrk ;;
      h2_proxy_tls)
        v_cfg=veil_proxy.toml;   v_url="https://127.0.0.1:${VEIL_HTTPS}${REQ_PATH}"
        n_url="https://127.0.0.1:${NGX_HTTPS}/proxy${REQ_PATH}"; tool=h2load ;;
      l4_tcp)
        v_cfg=veil_l4.toml;      v_url="http://127.0.0.1:${VEIL_L4}${REQ_PATH}"
        n_url="http://127.0.0.1:${NGX_L4}${REQ_PATH}"; tool=wrk ;;
      *) die "未知のシナリオ: $sc" ;;
    esac

    # ---- nginx ----
    log "[$sc iter=$iter] nginx 計測"
    r=$(measure "$tool" "$n_url")
    printf '%s\t%s\t%s\t%s\n' "$sc" nginx "$iter" "$r" >> "${OUT_TSV}"

    # ---- veil ----
    log "[$sc iter=$iter] veil 計測"
    start_veil "${WORK}/conf/${v_cfg}"
    case "$sc" in
      h2c_file_plain)               wait_port "${VEIL_HTTP}"  || die "veil(h2c) 起動失敗" ;;
      l4_tcp)                       wait_port "${VEIL_L4}"    || die "veil(l4) 起動失敗" ;;
      h3_file)                      wait_udp_port "${VEIL_HTTPS}" || die "veil(h3) 起動失敗" ;;
      *)                            wait_port "${VEIL_HTTPS}" || die "veil(https) 起動失敗" ;;
    esac
    r=$(measure "$tool" "$v_url")
    printf '%s\t%s\t%s\t%s\n' "$sc" veil "$iter" "$r" >> "${OUT_TSV}"
    stop_veil
    sleep 1
}

measure() {
    case "$1" in
        wrk)       run_wrk "$2" ;;
        h2load)    run_h2load "$2" ;;
        h2load_h3)
            # FreeBSD の nghttp2 pkg の h2load は ngtcp2 非搭載で QUIC を計測できないため、
            # quinn + h3 ベースの自前クライアント h3load（tools/perf/h3load）を優先する。
            # 未ビルドの場合のみ `h2load --h3`（= --alpn-list=h3 + QUIC 強制）へ
            # フォールバックする（QUIC 非対応ビルドでは計測失敗になる）。
            if [ -x "${H3LOAD_BIN}" ]; then
                run_h3load "$2"
            else
                log "警告: ${H3LOAD_BIN} が無いため h2load --h3 にフォールバック（QUIC 非対応ビルドだと失敗する）"
                run_h2load "$2" --h3
            fi
            ;;
    esac
}

# ---------------------------------------------------------------------------
# main
# ---------------------------------------------------------------------------
trap 'stop_all' EXIT INT TERM

prepare
stop_all
start_nginx

printf 'scenario\tserver\titer\trps\tmbps\tp50_ms\tp99_ms\terrors\n' > "${OUT_TSV}"

i=1
while [ "$i" -le "${ITERATIONS}" ]; do
    for sc in ${SCENARIOS}; do
        run_scenario "$sc" "$i"
    done
    i=$((i + 1))
done

log "完了: ${OUT_TSV}"
column -t "${OUT_TSV}" 2>/dev/null || cat "${OUT_TSV}"
