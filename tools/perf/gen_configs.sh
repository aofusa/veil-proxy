#!/usr/bin/env bash
# Veil パフォーマンス計測用の config.toml バリアントを生成する。
#
# full features ビルドのイメージ (veil:glibc / veil:musl) に対し、
# http2 / ktls / reuseport balancing / open_file_cache の 4 因子を **完全直交（2^4=16）**
# で組み合わせる。どのバリアントも同一の静的ファイル (/var/www/index.html) を File アクションで配信する。
#
# バリアント名: h2_<0|1>_ktls_<0|1>_lb_<cbpf|kernel>_ofc_<0|1>
#   （run_perf.sh は名前の "h2_1" から HTTP/2 負荷の要否を判定する）
#
# 使い方: bash tools/perf/gen_configs.sh
#   環境変数 KEEP_EXISTING=1 で既存 *.toml を消さずに追記する（既定は再生成前にクリア）。
set -euo pipefail

OUT="$(cd "$(dirname "$0")" && pwd)/configs"
mkdir -p "$OUT"

# 0/1 -> false/true（TOML bool へ）
b() { [ "$1" = 1 ] && echo true || echo false; }
# true -> True / false -> False（コメント表記）
cap() { case "$1" in true) echo True;; false) echo False;; *) echo "$1";; esac; }

# cfg <http2> <ktls> <lb> <ofc>  -> config.toml 本文を標準出力へ
cfg() {
    local http2="$1" ktls="$2" lb="$3" ofc="$4"
    local ofc_section=""
    if [ "$ofc" = "true" ]; then
        ofc_section=$'[route.open_file_cache]\nenabled = true\nvalid_duration_secs = 60\nmax_entries = 10000\n'
    fi
    cat <<EOF
# 自動生成: Veil パフォーマンス計測用バリアント
# http2=$(cap "$http2") ktls=$(cap "$ktls") lb=${lb} open_file_cache=$(cap "$ofc")
[server]
listen = "0.0.0.0:443"
http = "0.0.0.0:80"
http2_enabled = ${http2}
threads = 0

[logging]
# アクセスログ (INFO) はホットパスのオーバーヘッドになるため計測中は抑止（nginx も access_log off で公平化）
level = "warn"

[security]
allow_security_failures = false
drop_privileges_user = "nonroot"
drop_privileges_group = "nonroot"
enable_seccomp = true
enable_landlock = true
enable_sandbox = false
seccomp_mode = "filter"
landlock_read_paths = ["/var/www", "/var/cache/veil", "/var/tmp/veil"]
landlock_write_paths = ["/var/cache/veil", "/var/tmp/veil"]

[performance]
huge_pages_enabled = false
reuseport_balancing = "${lb}"

[tls]
cert_path = "/etc/veil/ssl/cert.pem"
key_path = "/etc/veil/ssl/key.pem"
ktls_enabled = ${ktls}
ktls_fallback_enabled = true

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/var/www/"
[route.security]
allowed_methods = ["HEAD", "GET"]
EOF
    printf '%s' "$ofc_section"
}

# 既存の生成物をクリア（_debug*.toml は .gitignore 対象・保持）
if [ "${KEEP_EXISTING:-0}" != 1 ]; then
    find "$OUT" -maxdepth 1 -name '*.toml' ! -name '_debug*.toml' -delete 2>/dev/null || true
fi

# 完全直交 2^4=16: http2 × ktls × lb(cbpf/kernel) × open_file_cache
count=0
for h2 in 0 1; do
    for ktls in 0 1; do
        for lb in cbpf kernel; do
            for ofc in 0 1; do
                name="h2_${h2}_ktls_${ktls}_lb_${lb}_ofc_${ofc}"
                path="$OUT/${name}.toml"
                cfg "$(b "$h2")" "$(b "$ktls")" "$lb" "$(b "$ofc")" > "$path"
                echo "wrote $path"
                count=$((count + 1))
            done
        done
    done
done
# ============================================================
# full features 機能ショーケース（default=ktls/http2 に加えて full で有効化される
# compression / cache / buffering / 逆プロキシ経路のスループットを計測する）。
# いずれも共通基盤（http2=on, ktls=off, kernel LB）上に 1 機能を重ねる。
# ※ kTLS はコンテナ環境と相性が悪いため feat 系構成では既定オフ
#   （kTLS 自体の影響は直交表 16 構成の ktls 因子で計測する）。
# 名前を h2_1_* にして run_perf.sh が HTTP/2 負荷も実施するようにする。
# proxy / buffering は上流 backend（run_perf.sh が起動する perf-backend）へ中継する。
# ============================================================

feat_base_head() {
    cat <<'EOF'
[server]
listen = "0.0.0.0:443"
http = "0.0.0.0:80"
http2_enabled = true
threads = 0

[logging]
level = "warn"

[security]
allow_security_failures = false
drop_privileges_user = "nonroot"
drop_privileges_group = "nonroot"
enable_seccomp = true
enable_landlock = true
enable_sandbox = false
seccomp_mode = "filter"
landlock_read_paths = ["/var/www", "/var/cache/veil", "/var/tmp/veil", "/etc/veil", "/etc/resolv.conf", "/etc/hosts", "/etc/nsswitch.conf", "/lib", "/lib64", "/usr"]
landlock_write_paths = ["/var/cache/veil", "/var/tmp/veil"]

[performance]
huge_pages_enabled = false
reuseport_balancing = "kernel"

[tls]
cert_path = "/etc/veil/ssl/cert.pem"
key_path = "/etc/veil/ssl/key.pem"
ktls_enabled = false
ktls_fallback_enabled = true
EOF
}

# 逆プロキシ系（proxy / buffering）が中継する上流定義。
feat_upstream() {
    cat <<'EOF'

[upstreams."perf-backend"]
algorithm = "round_robin"
servers = ["http://perf-backend:80/"]
EOF
}

# compression: 静的ファイル + レスポンス圧縮（Accept-Encoding 時に zstd/br/gzip）
{
    feat_base_head
    cat <<'EOF'

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/var/www/"
[route.compression]
enabled = true
preferred_encodings = ["zstd", "br", "gzip"]
min_size = 256
[route.security]
allowed_methods = ["HEAD", "GET"]
EOF
} > "$OUT/h2_1_feat_compression.toml"
echo "wrote $OUT/h2_1_feat_compression.toml"
count=$((count + 1))

# cache: 静的ファイル + インメモリキャッシュ（GET/200）
{
    feat_base_head
    cat <<'EOF'

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/var/www/"
[route.cache]
enabled = true
default_ttl_secs = 60
methods = ["GET"]
cacheable_statuses = [200]
[route.security]
allowed_methods = ["HEAD", "GET"]
EOF
} > "$OUT/h2_1_feat_cache.toml"
echo "wrote $OUT/h2_1_feat_cache.toml"
count=$((count + 1))

# proxy: 逆プロキシ経路（backend へ中継。プロキシのホットパス）
{
    feat_base_head
    feat_upstream
    cat <<'EOF'

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "Proxy"
upstream = "perf-backend"
[route.security]
allowed_methods = ["HEAD", "GET"]
EOF
} > "$OUT/h2_1_feat_proxy.toml"
echo "wrote $OUT/h2_1_feat_proxy.toml"
count=$((count + 1))

# buffering: 逆プロキシ + 高度なバッファリング制御（full buffering）
{
    feat_base_head
    feat_upstream
    cat <<'EOF'

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "Proxy"
upstream = "perf-backend"
[route.buffering]
mode = "full"
[route.security]
allowed_methods = ["HEAD", "GET"]
EOF
} > "$OUT/h2_1_feat_buffering.toml"
echo "wrote $OUT/h2_1_feat_buffering.toml"
count=$((count + 1))

# ============================================================
# full features 機能ショーケース第 2 弾（F-89: perf_measurement_report.md 指摘分）。
# wasm / metrics / access-log / rate-limit / admin / opentelemetry / l4-proxy を
# それぞれ 1 機能だけベース構成へ重ね、機能単位のオーバーヘッドを計測する。
# http3 / grpc-full / websocket は専用クライアント（h2load QUIC / k6）を第 3 弾で追加。
# ============================================================

# wasm: パススルー Proxy-Wasm フィルタ 1 枚（wasmtime 呼び出しの素のオーバーヘッド）。
# モジュールは run_perf.sh が docker/assets/wasm を /etc/veil/wasm へマウントする。
{
    feat_base_head
    cat <<'EOF'

[wasm]
enabled = true

[[wasm.modules]]
name = "passthrough"
path = "/etc/veil/wasm/passthrough_filter.wasm"

[wasm.modules.capabilities]
allow_logging = true

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/var/www/"
modules = ["passthrough"]
[route.security]
allowed_methods = ["HEAD", "GET"]
EOF
} > "$OUT/h2_1_feat_wasm.toml"
echo "wrote $OUT/h2_1_feat_wasm.toml"
count=$((count + 1))

# metrics: Prometheus メトリクス（リクエストごとのカウンタ/ヒストグラム更新コスト）
{
    feat_base_head
    cat <<'EOF'

[prometheus]
enabled = true
allowed_ips = ["127.0.0.1"]

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/var/www/"
[route.security]
allowed_methods = ["HEAD", "GET"]
EOF
} > "$OUT/h2_1_feat_metrics.toml"
echo "wrote $OUT/h2_1_feat_metrics.toml"
count=$((count + 1))

# access-log: JSON 構造化アクセスログ（フォーマット + 非同期ファイル出力コスト）。
# 出力先は tmpfs（/var/tmp/veil）でディスク I/O 自体は計測対象外、ホットパス側の
# フォーマット・チャネル送信コストを見る。
{
    feat_base_head
    cat <<'EOF'

[access_log]
enabled = true
format = "json"
file_path = "/var/tmp/veil/access.log"

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/var/www/"
[route.security]
allowed_methods = ["HEAD", "GET"]
EOF
} > "$OUT/h2_1_feat_access_log.toml"
echo "wrote $OUT/h2_1_feat_access_log.toml"
count=$((count + 1))

# rate-limit: スライディングウィンドウ判定コスト（上限は負荷を大きく上回る値にして
# 429 を発生させない。判定・状態更新のオーバーヘッドのみを見る）
{
    feat_base_head
    cat <<'EOF'

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/var/www/"
[route.security]
allowed_methods = ["HEAD", "GET"]
rate_limit_requests_per_min = 100000000
EOF
} > "$OUT/h2_1_feat_rate_limit.toml"
echo "wrote $OUT/h2_1_feat_rate_limit.toml"
count=$((count + 1))

# admin: Admin API 有効化時の通常リクエストへのルーティング判定オーバーヘッド
{
    feat_base_head
    cat <<'EOF'

[admin]
enabled = true
path_prefix = "/__admin"
secret = "perf-admin-secret"
allowed_ips = ["127.0.0.1"]

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/var/www/"
[route.security]
allowed_methods = ["HEAD", "GET"]
EOF
} > "$OUT/h2_1_feat_admin.toml"
echo "wrote $OUT/h2_1_feat_admin.toml"
count=$((count + 1))

# opentelemetry: OTLP/HTTP エクスポートスレッド動作時のデータプレーン干渉
# （metrics 併用で Prometheus レジストリをブリッジ）。エクスポート先は実コレクタ
# ではなく perf-backend（404 応答）で、収集・直列化・送信の干渉のみを見る。
{
    feat_base_head
    cat <<'EOF'

[prometheus]
enabled = true
allowed_ips = ["127.0.0.1"]

[opentelemetry]
enabled = true
endpoint = "http://perf-backend:80"
service_name = "veil-perf"
batch_interval_secs = 5

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/var/www/"
[route.security]
allowed_methods = ["HEAD", "GET"]
EOF
} > "$OUT/h2_1_feat_otel.toml"
echo "wrote $OUT/h2_1_feat_otel.toml"
count=$((count + 1))

# l4-proxy: L4 TCP ストリームプロキシ（perf-backend:80 へ素通し転送）。
# 平文 HTTP/1.1 負荷を 9080 の L4 リスナー経由で流す（run_perf.sh が URL を切替）。
# 443 の File ルートは readiness チェック用。
{
    feat_base_head
    cat <<'EOF'

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/var/www/"
[route.security]
allowed_methods = ["HEAD", "GET"]

[[l4]]
name = "perf-l4"
listen = "0.0.0.0:9080"
lb = "round_robin"
tls = "none"

  [[l4.upstreams]]
  addr = "perf-backend:80"
EOF
} > "$OUT/h2_0_feat_l4.toml"
echo "wrote $OUT/h2_0_feat_l4.toml"
count=$((count + 1))

# ============================================================
# full features 機能ショーケース第 3 弾（F-89 残件: 専用負荷クライアント導入分）。
# http3 / grpc-full / websocket を計測する。
#   - http3    : h2load の QUIC モード（--alpn-list=h3）で veil の静的配信を計測
#   - grpc     : k6(gRPC) → veil(TLS h2) → grpcbin(h2c) の unary 呼び出し
#   - websocket: k6(WS)   → veil(TLS)    → echo-server の WS エコー
# run_perf.sh が構成名から専用クライアント・上流バックエンドを判定する。
# ============================================================

# http3: HTTP/3 (QUIC) 有効化。UDP 443 で同一静的ファイルを配信し、
# h2load の QUIC モードで計測する（kTLS は QUIC 非対応のため既定どおりオフ）。
# feat_base_head は http3_enabled を持たないため server ブロックを個別生成する。
{
    cat <<'EOF'
[server]
listen = "0.0.0.0:443"
http = "0.0.0.0:80"
http2_enabled = true
http3_enabled = true
threads = 0

[logging]
level = "warn"

[security]
allow_security_failures = false
drop_privileges_user = "nonroot"
drop_privileges_group = "nonroot"
enable_seccomp = true
enable_landlock = true
enable_sandbox = false
seccomp_mode = "filter"
landlock_read_paths = ["/var/www", "/var/cache/veil", "/var/tmp/veil", "/etc/veil", "/etc/resolv.conf", "/etc/hosts", "/etc/nsswitch.conf", "/lib", "/lib64", "/usr"]
landlock_write_paths = ["/var/cache/veil", "/var/tmp/veil"]

[performance]
huge_pages_enabled = false
reuseport_balancing = "kernel"

[tls]
cert_path = "/etc/veil/ssl/cert.pem"
key_path = "/etc/veil/ssl/key.pem"
ktls_enabled = false
ktls_fallback_enabled = true

[http3]
listen = "0.0.0.0:443"

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/var/www/"
[route.security]
allowed_methods = ["HEAD", "GET"]
EOF
} > "$OUT/h2_1_feat_http3.toml"
echo "wrote $OUT/h2_1_feat_http3.toml"
count=$((count + 1))

# grpc: gRPC 逆プロキシ経路（veil で TLS 終端 → h2c で grpcbin へ中継）。
# k6 の gRPC クライアントが hello.HelloService/SayHello を veil 経由で呼ぶ。
# 上流 grpcbin は perf-grpc:9000（h2c）。use_h2c=true で HTTP/2 平文中継。
{
    feat_base_head
    cat <<'EOF'

# k6 の gRPC クライアントは完了ストリームごとに RST_STREAM(CANCEL) を送るため、
# 高 QPS では CVE-2023-44487 対策の RST レート制限（既定 100/s）に達し ENHANCE_YOUR_CALM で
# 接続が切れる。計測では判定・中継のオーバーヘッドを見たいので上限を大きく引き上げる
# （feat_rate_limit が 429 を避けるのと同じ方針。既定値の妥当性は本計測の対象外）。
[http2]
max_rst_stream_per_second = 100000000

# gRPC 上流（grpcbin の h2c gRPC）。veil は Content-Type: application/grpc を検出すると
# ルート `/*` プレフィックスを剥がさず完全パス（/hello.HelloService/SayHello）を保持して
# 中継する（B-40 修正）。use_h2c=true で HTTP/2 平文（h2c）中継。
[upstreams."perf-grpc"]
algorithm = "round_robin"
servers = ["http://perf-grpc:9000/"]

[[route]]
[route.conditions]
path = "/hello.HelloService/*"
[route.action]
type = "Proxy"
upstream = "perf-grpc"
use_h2c = true
[route.security]
allowed_methods = ["POST"]

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/var/www/"
[route.security]
allowed_methods = ["HEAD", "GET"]
EOF
} > "$OUT/h2_1_feat_grpc.toml"
echo "wrote $OUT/h2_1_feat_grpc.toml"
count=$((count + 1))

# websocket: WebSocket 逆プロキシ経路（Upgrade 自動検出 → echo-server へ中継）。
# k6 の WebSocket クライアントが /.ws でエコーサーバとフレームを往復する。
# 上流 echo-server は perf-ws:8080（平文 HTTP/1.1 + WS Upgrade）。
{
    feat_base_head
    cat <<'EOF'

[upstreams."perf-ws"]
algorithm = "round_robin"
servers = ["http://perf-ws:8080/"]

[[route]]
[route.conditions]
path = "/.ws"
[route.action]
type = "Proxy"
upstream = "perf-ws"
[route.security]
allowed_methods = ["GET"]

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/var/www/"
[route.security]
allowed_methods = ["HEAD", "GET"]
EOF
} > "$OUT/h2_1_feat_websocket.toml"
echo "wrote $OUT/h2_1_feat_websocket.toml"
count=$((count + 1))

# ============================================================
# F-112: 全プロトコル×全機能 網羅マトリクス（docs/artifacts/perf_coverage_report.md 起点）。
# 既存の File+機能（第2〜3弾）に加え、以下の欠落組み合わせをヘルパー関数で生成する。
#   グループA: HTTP/1.1&HTTP/2 + Proxy + 各機能
#   グループB: HTTP/3 + File / Proxy + 各機能
#   グループC: gRPC(over H2/H3) + 各機能
# 命名規則: h2_1_proxy_<feat> / h3_file_<feat> / h3_proxy[_<feat>] / grpc_h2_<feat> / grpc_h3[_<feat>]
#   （run_perf.sh がこの命名からクライアント種別・上流を判定する）
# ※ L4 + metrics/access_log/rate_limit（レポートのグループD）は L4ListenerConfig に
#   per-listener の該当設定が無く（L7 の [route.*] / グローバル [prometheus] 側の責務）、
#   設定として表現できないため N/A（L4 ベース 1 種のみを維持）。
# ============================================================

# server/logging/security/performance/tls 共通ヘッダ（http3 有無を引数化）。$1=http3(0/1)
gen_srv_head() {
    local h3="$1" http3_line="" http3_block=""
    if [ "$h3" = 1 ]; then
        http3_line=$'http3_enabled = true\n'
        http3_block=$'\n[http3]\nlisten = "0.0.0.0:443"\n'
    fi
    cat <<EOF
[server]
listen = "0.0.0.0:443"
http = "0.0.0.0:80"
http2_enabled = true
${http3_line}threads = 0

[logging]
level = "warn"

[security]
allow_security_failures = false
drop_privileges_user = "nonroot"
drop_privileges_group = "nonroot"
enable_seccomp = true
enable_landlock = true
enable_sandbox = false
seccomp_mode = "filter"
landlock_read_paths = ["/var/www", "/var/cache/veil", "/var/tmp/veil", "/etc/veil", "/etc/resolv.conf", "/etc/hosts", "/etc/nsswitch.conf", "/lib", "/lib64", "/usr"]
landlock_write_paths = ["/var/cache/veil", "/var/tmp/veil"]

[performance]
huge_pages_enabled = false
reuseport_balancing = "kernel"

[tls]
cert_path = "/etc/veil/ssl/cert.pem"
key_path = "/etc/veil/ssl/key.pem"
ktls_enabled = false
ktls_fallback_enabled = true
EOF
    printf '%s' "$http3_block"
}

# 機能ごとのトップレベルセクション（route 外に置く必要があるもの）。$1=feat
gen_top_section() {
    case "$1" in
        wasm)
            printf '\n[wasm]\nenabled = true\n\n[[wasm.modules]]\nname = "passthrough"\npath = "/etc/veil/wasm/passthrough_filter.wasm"\n\n[wasm.modules.capabilities]\nallow_logging = true\n'
            ;;
        metrics)
            printf '\n[prometheus]\nenabled = true\nallowed_ips = ["127.0.0.1"]\n'
            ;;
        access_log)
            printf '\n[access_log]\nenabled = true\nformat = "json"\nfile_path = "/var/tmp/veil/access.log"\n'
            ;;
        admin)
            printf '\n[admin]\nenabled = true\npath_prefix = "/__admin"\nsecret = "perf-admin-secret"\nallowed_ips = ["127.0.0.1"]\n'
            ;;
        otel)
            printf '\n[prometheus]\nenabled = true\nallowed_ips = ["127.0.0.1"]\n\n[opentelemetry]\nenabled = true\nendpoint = "http://perf-backend:80"\nservice_name = "veil-perf"\nbatch_interval_secs = 5\n'
            ;;
    esac
}

# 1 route ブロックを生成（TOML の table 順序: conditions→action→機能table→security）。
# $1=action(file|proxy) $2=upstream名 $3=path $4=feat $5=use_h2c(0/1) $6=methods(TOML配列中身)
gen_route() {
    local action="$1" upstream="$2" path="$3" feat="$4" h2c="$5" methods="$6"
    echo ""
    echo "[[route]]"
    echo "[route.conditions]"
    echo "path = \"$path\""
    echo "[route.action]"
    if [ "$action" = file ]; then
        echo 'type = "File"'
        echo 'path = "/var/www/"'
    else
        echo 'type = "Proxy"'
        echo "upstream = \"$upstream\""
        [ "$h2c" = 1 ] && echo "use_h2c = true"
    fi
    [ "$feat" = wasm ] && echo 'modules = ["passthrough"]'
    case "$feat" in
        cache)
            echo "[route.cache]"
            echo "enabled = true"
            echo "default_ttl_secs = 60"
            echo 'methods = ["GET"]'
            echo "cacheable_statuses = [200]"
            ;;
        compression)
            echo "[route.compression]"
            echo "enabled = true"
            echo 'preferred_encodings = ["zstd", "br", "gzip"]'
            echo "min_size = 256"
            ;;
        buffering)
            echo "[route.buffering]"
            echo 'mode = "full"'
            ;;
    esac
    echo "[route.security]"
    echo "allowed_methods = [$methods]"
    [ "$feat" = rate_limit ] && echo "rate_limit_requests_per_min = 100000000"
    return 0
}

# perf-backend 上流定義（proxy/buffering 用）
gen_upstream_backend() {
    printf '\n[upstreams."perf-backend"]\nalgorithm = "round_robin"\nservers = ["http://perf-backend:80/"]\n'
}
# perf-grpc 上流定義（gRPC 用）+ RST レート上限緩和（feat_grpc と同方針）
gen_grpc_prelude() {
    printf '\n[http2]\nmax_rst_stream_per_second = 100000000\n\n[upstreams."perf-grpc"]\nalgorithm = "round_robin"\nservers = ["http://perf-grpc:9000/"]\n'
}

emit_cfg() { # $1=path ; body は stdin（パイプ subshell 実行のため親 count は末尾でファイル数から再計算）
    cat > "$1"
    echo "wrote $1"
}

# --- グループA: HTTP/1.1 & HTTP/2 + Proxy + 各機能 ---
for feat in cache compression wasm metrics access_log rate_limit otel; do
    {
        gen_srv_head 0
        gen_upstream_backend
        gen_top_section "$feat"
        gen_route proxy perf-backend "/" "$feat" 0 '"HEAD", "GET"'
    } | emit_cfg "$OUT/h2_1_proxy_${feat}.toml"
done

# --- グループB-1: HTTP/3 + File + 各機能 ---
for feat in cache compression wasm metrics access_log rate_limit admin otel; do
    {
        gen_srv_head 1
        gen_top_section "$feat"
        gen_route file "" "/" "$feat" 0 '"HEAD", "GET"'
    } | emit_cfg "$OUT/h3_file_${feat}.toml"
done

# --- グループB-2: HTTP/3 + Proxy（ベース + 各機能） ---
{
    gen_srv_head 1
    gen_upstream_backend
    gen_route proxy perf-backend "/" none 0 '"HEAD", "GET"'
} | emit_cfg "$OUT/h3_proxy.toml"
for feat in buffering cache compression wasm metrics access_log rate_limit otel; do
    {
        gen_srv_head 1
        gen_upstream_backend
        gen_top_section "$feat"
        gen_route proxy perf-backend "/" "$feat" 0 '"HEAD", "GET"'
    } | emit_cfg "$OUT/h3_proxy_${feat}.toml"
done

# --- グループC-1: gRPC over HTTP/2 + 各機能 ---
# gRPC ルート（/hello.HelloService/*, use_h2c）へ機能を重ね、フォールバック File ルートを付す。
for feat in wasm metrics access_log rate_limit otel; do
    {
        gen_srv_head 0
        gen_grpc_prelude
        gen_top_section "$feat"
        gen_route proxy perf-grpc "/hello.HelloService/*" "$feat" 1 '"POST"'
        gen_route file "" "/" none 0 '"HEAD", "GET"'
    } | emit_cfg "$OUT/grpc_h2_${feat}.toml"
done

# --- グループC-2: gRPC over HTTP/3（ベース + 各機能） ---
# k6 は gRPC over H3 をネイティブ非対応のため run_perf.sh がフェイルセーフで NA を出す。
# 設定自体は http3_enabled + gRPC ルートで生成しておく（将来クライアント対応時に流用）。
{
    gen_srv_head 1
    gen_grpc_prelude
    gen_route proxy perf-grpc "/hello.HelloService/*" none 1 '"POST"'
    gen_route file "" "/" none 0 '"HEAD", "GET"'
} | emit_cfg "$OUT/grpc_h3.toml"
for feat in wasm metrics access_log rate_limit otel; do
    {
        gen_srv_head 1
        gen_grpc_prelude
        gen_top_section "$feat"
        gen_route proxy perf-grpc "/hello.HelloService/*" "$feat" 1 '"POST"'
        gen_route file "" "/" none 0 '"HEAD", "GET"'
    } | emit_cfg "$OUT/grpc_h3_${feat}.toml"
done

count=$(find "$OUT" -maxdepth 1 -name '*.toml' ! -name '_debug*.toml' | wc -l)
echo "生成完了: ${count} バリアント -> $OUT"
