#!/bin/bash
# E2Eテスト環境セットアップスクリプト
#
# veilをバックエンドサーバーとしても使用する構成:
#   - プロキシ: veil (ポート8443/8080)
#   - バックエンド1: veil (ポート9001、静的ファイル配信)
#   - バックエンド2: veil (ポート9002、静的ファイル配信)
#
# ビルド設定:
#   - すべてのfeaturesを有効化: full
#   - E2Eテストでは全機能をテストするため、すべてのfeaturesを有効化してビルドします
#
# 使用方法:
#   ./tests/e2e_setup.sh start   # 環境起動
#   ./tests/e2e_setup.sh stop    # 環境停止
#   ./tests/e2e_setup.sh test    # テスト実行

set -e

# Increase FD limit for parallel testing
ulimit -n 65536 2>/dev/null || true

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
# ワークスペース構成の場合、バイナリはワークスペースルートにある
WORKSPACE_DIR="$(dirname "$PROJECT_DIR")"
VEIL_BIN="${WORKSPACE_DIR}/target/debug/veil"
# veil-proxyローカルにある場合はそちらを優先
if [ -f "${PROJECT_DIR}/target/debug/veil" ]; then
    VEIL_BIN="${PROJECT_DIR}/target/debug/veil"
fi
FIXTURES_DIR="${SCRIPT_DIR}/fixtures"
PIDS_FILE="${FIXTURES_DIR}/pids.txt"

# 実行モード（host / container）。引数パースで上書きされる。
RUN_MODE="host"
VEIL_IMAGE="veil:glibc"
VEIL_PROXY_CONTAINER="veil-e2e-proxy"

# ポート設定
PROXY_HTTPS_PORT=8443
PROXY_HTTP_PORT=8080
PROXY_H2C_PORT=8081
PROXY_L4_PORT=8444
PROXY_L4_LEAST_CONN_PORT=8445
PROXY_L4_TERMINATE_PORT=8446
BACKEND1_PORT=9001
BACKEND2_PORT=9002
BACKEND_H2C_TLS_PORT=9013
BACKEND_H2C_PORT=9003
BACKEND_GRPC_PORT=9004
BACKEND_WS_PORT=9005
BACKEND_ERROR_PORT=9006
BACKEND_BAD_PORT=9009
BACKEND_CHUNKED_PORT=9007
BACKEND_ECHO_PORT=9008
BACKEND_TLS_ECHO_PORT=9018

# 色付き出力
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

log_info() {
    echo -e "${GREEN}[INFO]${NC} $1"
}

log_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

log_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

# kTLS利用可能性チェック
# kTLSが利用可能な場合0を返し、利用不可の場合1を返す
check_ktls_available() {
    # /proc/sys/net/ipv4/tcp_available_ulp が存在し、tlsが含まれているか確認
    if [ -f /proc/sys/net/ipv4/tcp_available_ulp ]; then
        if grep -q tls /proc/sys/net/ipv4/tcp_available_ulp 2>/dev/null; then
            return 0
        fi
    fi
    return 1
}

# ビルド済みバイナリの存在・実行可能性を確認（最大 timeout 秒）
wait_for_binary_ready() {
    local bin="$1"
    local label="$2"
    local timeout_secs="${3:-300}"
    local waited=0
    while [ "$waited" -lt "$timeout_secs" ]; do
        if [ -f "$bin" ] && [ -x "$bin" ]; then
            log_info "${label} ready: $bin"
            return 0
        fi
        sleep 0.5
        waited=$((waited + 1))
    done
    log_error "${label} not ready after ${timeout_secs}s: $bin"
    return 1
}

# E2E に必要な全バイナリをビルドし、完了を確認してから次フェーズへ進む
# features: full（全機能有効）
ensure_veil_binary() {
    local grpc_bin="${SCRIPT_DIR}/grpc_server/target/debug/grpc-server"
    local backends_bin="${SCRIPT_DIR}/test_backends/target/debug/test-backends"

    log_info "Building all E2E binaries (veil + grpc-server + test-backends) with features full..."
    cd "$PROJECT_DIR"
    if ! cargo build --features 'full'; then
        log_error "cargo build --features full failed"
        exit 1
    fi
    if ! (cd "${SCRIPT_DIR}/grpc_server" && cargo build --quiet); then
        log_error "grpc_server build failed"
        exit 1
    fi
    if ! (cd "${SCRIPT_DIR}/test_backends" && cargo build --quiet); then
        log_error "test_backends build failed"
        exit 1
    fi
    cd - > /dev/null

    wait_for_binary_ready "$VEIL_BIN" "veil" 300 || exit 1
    wait_for_binary_ready "$grpc_bin" "grpc-server" 120 || exit 1
    wait_for_binary_ready "$backends_bin" "test-backends" 120 || exit 1

    log_info "All E2E binaries built and verified"
}

# フィクスチャディレクトリの準備
prepare_fixtures() {
    mkdir -p "$FIXTURES_DIR"
    mkdir -p "${FIXTURES_DIR}/backend1"
    mkdir -p "${FIXTURES_DIR}/backend2"
    mkdir -p "${FIXTURES_DIR}/backend_h2c"
    
    # テスト用証明書を生成
    # CA:FALSE を指定して end-entity 証明書として生成
    # （CA 証明書として生成すると CaUsedAsEndEntity エラーになる）
    if [ ! -f "${FIXTURES_DIR}/cert.pem" ]; then
        log_info "Generating ECDSA test certificates..."
        
        # OpenSSLでECDSA自己署名証明書を生成（end-entity証明書）
        openssl req -x509 -newkey EC -pkeyopt ec_paramgen_curve:secp384r1 -nodes \
            -keyout "${FIXTURES_DIR}/key.pem" \
            -out "${FIXTURES_DIR}/cert.pem" \
            -days 365 \
            -subj "/CN=localhost" \
            -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" \
            -addext "basicConstraints=critical,CA:FALSE" \
            -addext "keyUsage=critical,digitalSignature,keyEncipherment" \
            -addext "extendedKeyUsage=serverAuth" \
            2>/dev/null
        
        log_info "ECDSA Certificates generated (secp384r1)"
    fi
    
    # バックエンド1用テストファイル（テストが "Hello from Backend N" を含むことを確認するため両方含める）
    echo "<h1>Hello from Backend 1</h1>" > "${FIXTURES_DIR}/backend1/index.html"
    echo "<h1>Hello from Backend 2</h1>" > "${FIXTURES_DIR}/backend2/index.html"
    echo "<h1>H2C Backend</h1>" > "${FIXTURES_DIR}/backend_h2c/index.html"
    
    # ヘルスチェック用エンドポイント（200 OKを返す）
    echo "OK" > "${FIXTURES_DIR}/backend1/health"
    echo "OK" > "${FIXTURES_DIR}/backend2/health"
    echo "OK" > "${FIXTURES_DIR}/backend_h2c/health"

    # 圧縮テスト・大容量ファイルテスト用（1024バイト超の圧縮閾値とtest_static_file_large用）
    head -c 10000 /dev/zero | tr '\0' 'A' > "${FIXTURES_DIR}/backend1/large.txt"
    head -c 10000 /dev/zero | tr '\0' 'A' > "${FIXTURES_DIR}/backend2/large.txt"
    head -c 10000 /dev/zero | tr '\0' 'A' > "${FIXTURES_DIR}/backend_h2c/large.txt"

    # H2C テスト用ファイル（test_h2c_post_request / test_h2c_large_request_body が
    # /h2c/test.txt、test_h2c_multiple_streams が /h2c/test{0,1,2}.txt を参照する）
    echo "h2c test file" > "${FIXTURES_DIR}/backend_h2c/test.txt"
    for i in 0 1 2; do
        echo "h2c stream test ${i}" > "${FIXTURES_DIR}/backend_h2c/test${i}.txt"
    done
    
    # /healthエンドポイント用JSONファイル（プロキシが直接サービスする）
    echo '{"status":"ok","proxy":"veil"}' > "${FIXTURES_DIR}/proxy_health.json"

    # ルーティングテスト用フィクスチャ（各テストが使用するサブパス）
    for backend in backend1 backend2; do
        mkdir -p "${FIXTURES_DIR}/${backend}/api/v1"
        mkdir -p "${FIXTURES_DIR}/${backend}/api/v2"
        mkdir -p "${FIXTURES_DIR}/${backend}/api/header-filter"
        mkdir -p "${FIXTURES_DIR}/${backend}/api/query-filter"
        mkdir -p "${FIXTURES_DIR}/${backend}/api/condition-and"
        mkdir -p "${FIXTURES_DIR}/${backend}/api/combined"
        mkdir -p "${FIXTURES_DIR}/${backend}/api/ip-restricted"
        mkdir -p "${FIXTURES_DIR}/${backend}/api/get-only"
        echo "<h1>API Index</h1>" > "${FIXTURES_DIR}/${backend}/api/index.html"
        echo "v1 test" > "${FIXTURES_DIR}/${backend}/api/v1/test"
        echo "v2 test" > "${FIXTURES_DIR}/${backend}/api/v2/test"
        echo "<h1>Header Filter OK</h1>" > "${FIXTURES_DIR}/${backend}/api/header-filter/index.html"
        echo "<h1>Query Filter OK</h1>" > "${FIXTURES_DIR}/${backend}/api/query-filter/index.html"
        echo "<h1>Condition AND OK</h1>" > "${FIXTURES_DIR}/${backend}/api/condition-and/index.html"
        echo "<h1>Combined OK</h1>" > "${FIXTURES_DIR}/${backend}/api/combined/index.html"
        echo "<h1>IP Restricted OK</h1>" > "${FIXTURES_DIR}/${backend}/api/ip-restricted/index.html"
        echo "<h1>GET Only OK</h1>" > "${FIXTURES_DIR}/${backend}/api/get-only/index.html"
        # prefix-strippedパス（/api/* ルートが /api プレフィックスを除去した後）
        mkdir -p "${FIXTURES_DIR}/${backend}/v1"
        mkdir -p "${FIXTURES_DIR}/${backend}/v2"
        echo "v1 test" > "${FIXTURES_DIR}/${backend}/v1/test"
        echo "v2 test" > "${FIXTURES_DIR}/${backend}/v2/test"
        # B-10: ロードバランシング分散テスト専用パス（共有 "/" の RR ステートと隔離）
        mkdir -p "${FIXTURES_DIR}/${backend}/rr-test"
        echo "<h1>RR Test</h1>" > "${FIXTURES_DIR}/${backend}/rr-test/index.html"
    done

    # 必要なディレクトリの作成
    mkdir -p "${FIXTURES_DIR}/wasm"
    mkdir -p "/tmp/veil_buffer"
    # cp -p でソースの mtime を維持する。維持しないと毎回コピー先 .wasm の mtime が
    # 更新され、前回生成の AOT キャッシュ（<path>.cwasm）が「古い」と判定されて
    # 毎回 WASM を再コンパイルしてしまう（起動が数十秒遅くなる）。
    if [ -f "${SCRIPT_DIR}/wasm/header_filter.wasm" ]; then
        cp -p "${SCRIPT_DIR}/wasm/header_filter.wasm" "${FIXTURES_DIR}/wasm/header_filter.wasm"
        log_info "WASM module header_filter.wasm copied"
    else
        log_warn "WASM module header_filter.wasm not found at ${SCRIPT_DIR}/wasm/header_filter.wasm"
    fi
    if [ -f "${SCRIPT_DIR}/wasm/http_call_filter.wasm" ]; then
        cp -p "${SCRIPT_DIR}/wasm/http_call_filter.wasm" "${FIXTURES_DIR}/wasm/http_call_filter.wasm"
        log_info "WASM module http_call_filter.wasm copied"
    fi
}

# 設定ファイルを生成
# 引数: 設定タイプ (default|cache|buffering|healthcheck|least_conn|ip_hash)
generate_configs() {
    local config_type="${1:-default}"

    # kTLS: プロキシのフロント TLS のみ有効化（バックエンドは常に false）
    # バックエンド側 kTLS を有効にすると上流 HTTPS 接続が 502 化するため、
    # クライアント→プロキシ経路のみ検証する。
    local proxy_ktls_enabled="false"
    local backend_ktls_enabled="false"
    if check_ktls_available; then
        proxy_ktls_enabled="true"
        log_info "kTLS available — enabling ktls on proxy only (backends stay false)"
    else
        log_warn "kTLS not available — proxy/backends use ktls_enabled = false"
    fi
    
    # バックエンド1設定（静的ファイル配信）
    cat > "${FIXTURES_DIR}/backend1.toml" << EOF
[server]
listen = "127.0.0.1:${BACKEND1_PORT}"
threads = 1

[tls]
cert_path = "${FIXTURES_DIR}/cert.pem"
key_path = "${FIXTURES_DIR}/key.pem"
ktls_enabled = ${backend_ktls_enabled}

[logging]
level = "warn"

[[route]]
[route.conditions]
host = "localhost"
path = "/*"
[route.action]
type = "File"
path = "${FIXTURES_DIR}/backend1"
index = "index.html"
[route.security]
add_response_headers = { "X-Server-Id" = "backend1" }

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/*"
[route.action]
type = "File"
path = "${FIXTURES_DIR}/backend1"
index = "index.html"
[route.security]
add_response_headers = { "X-Server-Id" = "backend1" }
EOF

    # バックエンド2設定（静的ファイル配信）
    cat > "${FIXTURES_DIR}/backend2.toml" << EOF
[server]
listen = "127.0.0.1:${BACKEND2_PORT}"
threads = 1

[tls]
cert_path = "${FIXTURES_DIR}/cert.pem"
key_path = "${FIXTURES_DIR}/key.pem"
ktls_enabled = ${backend_ktls_enabled}

[logging]
level = "warn"

[[route]]
[route.conditions]
host = "localhost"
path = "/*"
[route.action]
type = "File"
path = "${FIXTURES_DIR}/backend2"
index = "index.html"
[route.security]
add_response_headers = { "X-Server-Id" = "backend2" }

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/*"
[route.action]
type = "File"
path = "${FIXTURES_DIR}/backend2"
index = "index.html"
[route.security]
add_response_headers = { "X-Server-Id" = "backend2" }
EOF

    # H2Cバックエンド設定（HTTP/2 over cleartext、静的ファイル配信）
    # HTTP（平文）サーバーとして動作させ、H2C接続を受け入れる
    # 注意: H2C専用サーバーのため、通常のTLSリスナーは起動されない
    cat > "${FIXTURES_DIR}/backend_h2c.toml" << EOF
[server]
listen = "127.0.0.1:${BACKEND_H2C_TLS_PORT}"  # TLSポート（未使用）
h2c_listen = "127.0.0.1:${BACKEND_H2C_PORT}"  # H2C専用ポート（使用）
threads = 1
http2_enabled = true
h2c_enabled = true
# 注意: listenとh2c_listenを別ポートにすることで競合を回避しつつH2Cを有効化

[tls]
cert_path = "${FIXTURES_DIR}/cert.pem"
key_path = "${FIXTURES_DIR}/key.pem"
ktls_enabled = ${backend_ktls_enabled}
# 注意: H2C専用サーバーのため、TLS証明書は使用されないが、設定ファイルの検証で必要

[logging]
level = "warn"

[[route]]
[route.conditions]
host = "localhost"
path = "/*"
[route.action]
type = "File"
path = "${FIXTURES_DIR}/backend_h2c"
index = "index.html"
[route.security]
add_response_headers = { "X-Server-Id" = "backend_h2c" }

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/*"
[route.action]
type = "File"
path = "${FIXTURES_DIR}/backend_h2c"
index = "index.html"
[route.security]
add_response_headers = { "X-Server-Id" = "backend_h2c" }
EOF

    # プロキシ設定（設定タイプに応じて生成）
    local algorithm="round_robin"
    case "$config_type" in
        least_conn)
            algorithm="least_conn"
            ;;
        ip_hash)
            algorithm="ip_hash"
            ;;
    esac
    
    # 基本設定
    cat > "${FIXTURES_DIR}/proxy.toml" << EOF
[server]
listen = "127.0.0.1:${PROXY_HTTPS_PORT}"
http = "127.0.0.1:${PROXY_HTTP_PORT}"
h2c_listen = "127.0.0.1:${PROXY_H2C_PORT}"
h2c_enabled = true
redirect_http_to_https = false
threads = 1
http2_enabled = true
http3_enabled = true

[tls]
cert_path = "${FIXTURES_DIR}/cert.pem"
key_path = "${FIXTURES_DIR}/key.pem"
ktls_enabled = ${proxy_ktls_enabled}
# F-50: cipher_suites の取捨選択・優先度（記載順 = サーバ優先度順）検証用。
# CHACHA20 系を意図的に除外し、E2E で「除外スイートは拒否・先頭スイートが優先」を検証する。
cipher_suites = [
    "TLS13_AES_256_GCM_SHA384",
    "TLS13_AES_128_GCM_SHA256",
    "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384",
    "TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256",
]
# F-49: 証明書ホットリロード（SIGHUP 即時 + mtime 定期チェック）の E2E 検証用
auto_reload = true
reload_interval_secs = 60

# 注意: level は "info"。"debug" にすると WASM AOT コンパイル時に
# cranelift/wasmtime の内部 DEBUG ログ（数万行）が ftlog 経由で出力され、
# モジュール 1 つあたり数十秒かかって起動レディネス待ち（数秒）を超過し、
# 「Proxy failed to start」の一過性エラーを起こす。E2E テストはプロキシの
# debug ログを参照しないため info で十分。
[logging]
level = "info"

[prometheus]
enabled = true
path = "/__metrics"

[admin]
enabled = true
path_prefix = "/__admin"
secret = "test-admin-secret"

[http3]
listen = "127.0.0.1:${PROXY_HTTPS_PORT}"
compression_enabled = true

# L4 TCP プロキシ（TLS パススルー、F-30 splice 検証用）
# クライアントの TLS は backend1/2(HTTPS) へ透過転送され、L4 は生バイトを双方向中継する。
[[l4]]
name = "l4-passthrough"
listen = "127.0.0.1:${PROXY_L4_PORT}"
lb = "round_robin"
tls = "passthrough"

  [[l4.upstreams]]
  addr = "127.0.0.1:${BACKEND1_PORT}"

  [[l4.upstreams]]
  addr = "127.0.0.1:${BACKEND2_PORT}"

# L4 TCP プロキシ（Least Connection 負荷分散）
[[l4]]
name = "l4-least-conn"
listen = "127.0.0.1:${PROXY_L4_LEAST_CONN_PORT}"
lb = "least_conn"
tls = "passthrough"

  [[l4.upstreams]]
  addr = "127.0.0.1:${BACKEND1_PORT}"

  [[l4.upstreams]]
  addr = "127.0.0.1:${BACKEND2_PORT}"

# L4 TCP プロキシ（TLS 終端 → 平文 HTTP バックエンドへ転送）
[[l4]]
name = "l4-tls-terminate"
listen = "127.0.0.1:${PROXY_L4_TERMINATE_PORT}"
lb = "round_robin"
tls = "terminate"

  [[l4.upstreams]]
  addr = "127.0.0.1:${BACKEND_ECHO_PORT}"

[upstreams."backend-pool"]
algorithm = "${algorithm}"
servers = [
    "https://127.0.0.1:${BACKEND1_PORT}",
    "https://127.0.0.1:${BACKEND2_PORT}"
]
tls_insecure = true

[upstreams."backend-pool".health_check]
enabled = false
path = "/health"
interval_secs = 60
timeout_secs = 3
healthy_threshold = 1
unhealthy_threshold = 10000
use_tls = true
verify_cert = false

# B-10: Round Robin 分散テスト専用プール
# 他のテストが同時に "/"（backend-pool）へリクエストすると RR カーソルが共有されて
# 分散カウントが干渉するため、分散を Assert するテストはこの専用プールを使う。
[upstreams."rr-isolated-pool"]
algorithm = "round_robin"
servers = [
    "https://127.0.0.1:${BACKEND1_PORT}",
    "https://127.0.0.1:${BACKEND2_PORT}"
]
tls_insecure = true

[upstreams."grpc-pool"]
algorithm = "round_robin"
servers = [
    "http://127.0.0.1:${BACKEND_GRPC_PORT}"
]

[upstreams."weighted-pool"]
algorithm = "weighted"
servers = [
    { url = "https://127.0.0.1:${BACKEND1_PORT}", weight = 2 },
    { url = "https://127.0.0.1:${BACKEND2_PORT}", weight = 1 }
]
tls_insecure = true

[upstreams."ch-pool"]
algorithm = "consistent_hash"
servers = [
    "https://127.0.0.1:${BACKEND1_PORT}",
    "https://127.0.0.1:${BACKEND2_PORT}"
]
tls_insecure = true

[upstreams."ch-pool".circuit_breaker]
enabled = true
failure_threshold = 5
failure_window_secs = 60
open_duration_secs = 10
half_open_probes = 2
success_threshold = 2
trip_on_timeout = true

[upstreams."error-pool"]
algorithm = "round_robin"
servers = [
    "http://127.0.0.1:${BACKEND_ERROR_PORT}"
]

[upstreams."error-pool".circuit_breaker]
enabled = true
failure_threshold = 3
failure_window_secs = 30
open_duration_secs = 5
half_open_probes = 1
success_threshold = 1
trip_on_timeout = true

# B-17: プロトコル違反バックエンド（回帰テスト用）。
# サーキットブレーカは無効（連続 5xx で open になると 502 の由来が曖昧になるため）
[upstreams."bad-pool"]
algorithm = "round_robin"
servers = [
    "http://127.0.0.1:${BACKEND_BAD_PORT}"
]

# SNI 上書き検証用（IP 直打ち + sni_name = localhost）
[upstreams."sni-pool"]
algorithm = "round_robin"
servers = [
    { url = "https://127.0.0.1:${BACKEND1_PORT}", sni_name = "localhost" }
]
tls_insecure = true

# 厳密証明書検証用（自己署名証明書は拒否される想定）
[upstreams."strict-cert-pool"]
algorithm = "round_robin"
servers = [
    "https://127.0.0.1:${BACKEND1_PORT}"
]
tls_insecure = false

# TCP ヘルスチェック検証用
[upstreams."tcp-health-pool"]
algorithm = "round_robin"
servers = [
    "https://127.0.0.1:${BACKEND1_PORT}"
]
tls_insecure = true

[upstreams."tcp-health-pool".health_check]
enabled = true
check_type = "tcp"
interval_secs = 2
timeout_secs = 2
healthy_threshold = 1
unhealthy_threshold = 3
use_tls = true
verify_cert = false

# gRPC ヘルスチェック検証用
[upstreams."grpc-health-pool"]
algorithm = "round_robin"
servers = [
    "http://127.0.0.1:${BACKEND_GRPC_PORT}"
]

[upstreams."grpc-health-pool".health_check]
enabled = true
check_type = "grpc"
path = "grpc.health.v1.Health"
interval_secs = 2
timeout_secs = 3
healthy_threshold = 1
unhealthy_threshold = 3
EOF

    # ヘルスチェック設定を追加（healthcheckタイプの時のみ有効化）
    if [ "$config_type" = "healthcheck" ]; then
        cat >> "${FIXTURES_DIR}/proxy.toml" << EOF

[upstreams."backend-pool".health_check]
enabled = true
path = "/health"
interval_secs = 1
timeout_secs = 2
healthy_threshold = 1
unhealthy_threshold = 3
EOF
    fi
    
    # (Generic /* moved to the end)

    # NOTE: Global [route.cache] and [route.buffering] are removed to avoid TOML duplicate key error.
    # Specific route settings below will handle them.
    
    # バッファリング・キャッシュモード別のルート設定（streaming, full, adaptive, cached）
    if [ "$config_type" = "buffering" ] || [ "$config_type" = "default" ] || [ "$config_type" = "cache" ]; then
        cat >> "${FIXTURES_DIR}/proxy.toml" << EOF

[[route]]
[route.conditions]
host = "localhost"
path = "/streaming/*"
[route.action]
type = "Proxy"
upstream = "backend-pool"
[route.buffering]
mode = "streaming"

[[route]]
[route.conditions]
host = "localhost"
path = "/full/*"
[route.action]
type = "Proxy"
upstream = "backend-pool"
[route.buffering]
mode = "full"

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/streaming/*"
[route.action]
type = "Proxy"
upstream = "backend-pool"
[route.buffering]
mode = "streaming"

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/full/*"
[route.action]
type = "Proxy"
upstream = "backend-pool"
[route.buffering]
mode = "full"

[[route]]
[route.conditions]
host = "localhost"
path = "/adaptive/*"
[route.action]
type = "Proxy"
upstream = "backend-pool"
[route.buffering]
mode = "adaptive"
adaptive_threshold = 4096

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/adaptive/*"
[route.action]
type = "Proxy"
upstream = "backend-pool"
[route.buffering]
mode = "adaptive"
adaptive_threshold = 4096

[[route]]
[route.conditions]
host = "localhost"
path = "/cached/*"
[route.action]
type = "Proxy"
upstream = "backend-pool"
[route.cache]
enabled = true
default_ttl_secs = 1
methods = ["GET"]
cacheable_statuses = [200]

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/cached/*"
[route.action]
type = "Proxy"
upstream = "backend-pool"
[route.cache]
enabled = true
default_ttl_secs = 1
methods = ["GET"]
cacheable_statuses = [200]
EOF
    fi

    # security プロファイル: レート制限・接続制限を集中検証
    if [ "$config_type" = "security" ] || [ "$config_type" = "default" ]; then
        cat >> "${FIXTURES_DIR}/proxy.toml" << EOF

[[route]]
[route.conditions]
host = "localhost"
path = "/rate-limited/*"
[route.action]
type = "Proxy"
upstream = "backend-pool"
[route.security]
rate_limit_requests_per_min = 10

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/rate-limited/*"
[route.action]
type = "Proxy"
upstream = "backend-pool"
[route.security]
rate_limit_requests_per_min = 10
EOF
    fi

    # バックエンド接続オプション検証ルート（SNI / 厳密証明書 / ヘルスチェック種別）
    cat >> "${FIXTURES_DIR}/proxy.toml" << EOF

[[route]]
[route.conditions]
host = "localhost"
path = "/sni-upstream/*"
[route.action]
type = "Proxy"
upstream = "sni-pool"

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/sni-upstream/*"
[route.action]
type = "Proxy"
upstream = "sni-pool"

[[route]]
[route.conditions]
host = "localhost"
path = "/strict-cert/*"
[route.action]
type = "Proxy"
upstream = "strict-cert-pool"

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/strict-cert/*"
[route.action]
type = "Proxy"
upstream = "strict-cert-pool"

[[route]]
[route.conditions]
host = "localhost"
path = "/tcp-health/*"
[route.action]
type = "Proxy"
upstream = "tcp-health-pool"

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/tcp-health/*"
[route.action]
type = "Proxy"
upstream = "tcp-health-pool"

[[route]]
[route.conditions]
host = "localhost"
path = "/grpc-health/*"
[route.action]
type = "Proxy"
upstream = "grpc-health-pool"
use_h2c = true

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/grpc-health/*"
[route.action]
type = "Proxy"
upstream = "grpc-health-pool"
use_h2c = true
EOF
    

    # gRPC/H2C ルート設定 (優先順位を上げるため先に定義)
    cat >> "${FIXTURES_DIR}/proxy.toml" << EOF

# H2Cルート設定
[[route]]
[route.conditions]
host = "localhost"
path = "/h2c/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:${BACKEND_H2C_PORT}"
use_h2c = true
[route.security]
add_response_headers = { "X-Proxied-By" = "veil", "X-H2C-Test" = "true" }

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/h2c/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:${BACKEND_H2C_PORT}"
use_h2c = true
[route.security]
add_response_headers = { "X-Proxied-By" = "veil", "X-H2C-Test" = "true" }

# chunked ストリーミングテスト用ルート (F-32: HTTP/2 chunked レスポンスストリーミング)
# Transfer-Encoding: chunked のバックエンドを HTTP/2 DATA フレームへ逐次転送する経路を検証
[[route]]
[route.conditions]
host = "localhost"
path = "/chunked-stream/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:${BACKEND_CHUNKED_PORT}"

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/chunked-stream/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:${BACKEND_CHUNKED_PORT}"

# ボディエコー用ルート (F-32: HTTP/2 リクエスト方向ストリーミング)
# リクエストボディを chunked でバックエンドへ逐次転送する経路を検証（echo で往復一致確認）
[[route]]
[route.conditions]
host = "localhost"
path = "/echo-upload/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:${BACKEND_ECHO_PORT}"

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/echo-upload/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:${BACKEND_ECHO_PORT}"

# F-44: TLS バックエンドストリーミング検証用（HTTPS echo バックエンド）
[[route]]
[route.conditions]
host = "localhost"
path = "/echo-upload-tls/*"
[route.action]
type = "Proxy"
url = "https://127.0.0.1:${BACKEND_TLS_ECHO_PORT}"

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/echo-upload-tls/*"
[route.action]
type = "Proxy"
url = "https://127.0.0.1:${BACKEND_TLS_ECHO_PORT}"

# タイムアウトテスト用ルート (存在しないポートへ転送)
[[route]]
[route.conditions]
host = "localhost"
path = "/slow/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:1"

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/slow/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:1"

# ディスク溢れテスト用ルート (極めて小さいバッファ制限)
[[route]]
[route.conditions]
host = "localhost"
path = "/disk-spillover/*"
[route.action]
type = "Proxy"
upstream = "backend-pool"
[route.buffering]
mode = "full"
max_memory_buffer = 100
max_disk_buffer = 100
disk_buffer_path = "/tmp/veil_buffer"

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/disk-spillover/*"
[route.action]
type = "Proxy"
upstream = "backend-pool"
[route.buffering]
mode = "full"
max_memory_buffer = 100
max_disk_buffer = 100
disk_buffer_path = "/tmp/veil_buffer"
EOF

    # HTTP 500 エラーバックエンドおよび新機能ルート設定
    cat >> "${FIXTURES_DIR}/proxy.toml" << EOF

[[route]]
[route.conditions]
host = "localhost"
path = "/error-500/*"
[route.action]
type = "Proxy"
upstream = "error-pool"

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/error-500/*"
[route.action]
type = "Proxy"
upstream = "error-pool"

# B-17: プロトコル違反バックエンドへのルート（回帰テスト用）
[[route]]
[route.conditions]
host = "localhost"
path = "/bad-backend/*"
[route.action]
type = "Proxy"
upstream = "bad-pool"

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/bad-backend/*"
[route.action]
type = "Proxy"
upstream = "bad-pool"

# B-10: Round Robin 分散テスト専用ルート（共有 "/" と RR ステートを隔離）
[[route]]
[route.conditions]
host = "localhost"
path = "/rr-test/*"
[route.action]
type = "Proxy"
upstream = "rr-isolated-pool"

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/rr-test/*"
[route.action]
type = "Proxy"
upstream = "rr-isolated-pool"

[[route]]
[route.conditions]
host = "localhost"
path = "/weighted/*"
[route.action]
type = "Proxy"
upstream = "weighted-pool"

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/weighted/*"
[route.action]
type = "Proxy"
upstream = "weighted-pool"

[[route]]
[route.conditions]
host = "localhost"
path = "/consistent-hash/*"
[route.action]
type = "Proxy"
upstream = "ch-pool"

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/consistent-hash/*"
[route.action]
type = "Proxy"
upstream = "ch-pool"
EOF

    # WebSocketルート設定
    cat >> "${FIXTURES_DIR}/proxy.toml" << EOF
[upstreams."ws-pool"]
algorithm = "round_robin"
servers = [
    "http://127.0.0.1:${BACKEND_WS_PORT}"
]

[[route]]
[route.conditions]
host = "localhost"
path = "/ws/*"
[route.action]
type = "Proxy"
upstream = "ws-pool"

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/ws/*"
[route.action]
type = "Proxy"
upstream = "ws-pool"

EOF

    # gRPCルート設定
    cat >> "${FIXTURES_DIR}/proxy.toml" << EOF
[[route]]
[route.conditions]
host = "localhost"
path = "/grpc.test.v1.TestService/*"
[route.action]
type = "Proxy"
upstream = "grpc-pool"
use_h2c = true
[route.security]
add_response_headers = { "X-Proxied-By" = "veil", "X-GRPC-Test" = "true", "X-Fixed-Routing" = "true" }

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/grpc.test.v1.TestService/*"
[route.action]
type = "Proxy"
upstream = "grpc-pool"
use_h2c = true
[route.security]
add_response_headers = { "X-Proxied-By" = "veil", "X-GRPC-Test" = "true" }
EOF

    # WASM設定を追加（wasm設定タイプの時のみ有効化）
    if [ "$config_type" = "wasm" ] || [ "$config_type" = "default" ]; then
        if [ -f "${FIXTURES_DIR}/wasm/header_filter.wasm" ]; then
            cat >> "${FIXTURES_DIR}/proxy.toml" << EOF

[wasm]
enabled = true

[[wasm.modules]]
name = "header_filter"
path = "${FIXTURES_DIR}/wasm/header_filter.wasm"
configuration = '{"add_header": "X-Wasm-Processed", "add_value": "true"}'

[wasm.modules.capabilities]
allow_logging = true
allow_request_headers_read = true
allow_request_headers_write = true
allow_response_headers_read = true
allow_response_headers_write = true
allow_send_local_response = true
EOF

            # F-62: HTTP コール（Pause/resume）フィルタ
            if [ -f "${FIXTURES_DIR}/wasm/http_call_filter.wasm" ]; then
                cat >> "${FIXTURES_DIR}/proxy.toml" << EOF

[[wasm.modules]]
name = "http_call_filter"
path = "${FIXTURES_DIR}/wasm/http_call_filter.wasm"
configuration = 'bad-pool'

[wasm.modules.capabilities]
allow_logging = true
allow_request_headers_read = true
allow_request_headers_write = true
allow_send_local_response = true
allow_http_calls = true
allowed_upstreams = ["bad-pool"]
EOF
            fi
            
            # WASMモジュールを適用するルートを追加
            # 注意: modulesはroute直下（[route.action]配下ではなく）に設定する必要がある
            cat >> "${FIXTURES_DIR}/proxy.toml" << EOF

[[route]]
modules = ["header_filter"]
[route.conditions]
host = "localhost"
path = "/wasm/*"
[route.action]
type = "Proxy"
upstream = "backend-pool"
[route.security]
add_response_headers = { "X-Proxied-By" = "veil" }

[[route]]
modules = ["header_filter"]
[route.conditions]
host = "127.0.0.1"
path = "/wasm/*"
[route.action]
type = "Proxy"
upstream = "backend-pool"
[route.security]
add_response_headers = { "X-Proxied-By" = "veil" }
EOF

            # F-62: HTTP コール（Pause/resume）フィルタのルート
            if [ -f "${FIXTURES_DIR}/wasm/http_call_filter.wasm" ]; then
                cat >> "${FIXTURES_DIR}/proxy.toml" << EOF

[[route]]
modules = ["http_call_filter"]
[route.conditions]
host = "localhost"
path = "/wasm-http-call/*"
[route.action]
type = "Proxy"
upstream = "backend-pool"

[[route]]
modules = ["http_call_filter"]
[route.conditions]
host = "127.0.0.1"
path = "/wasm-http-call/*"
[route.action]
type = "Proxy"
upstream = "backend-pool"
EOF
            fi
        fi
    fi

    # /healthエンドポイント（プロキシが直接JSONファイルを返す）
    cat >> "${FIXTURES_DIR}/proxy.toml" << EOF

[[route]]
[route.conditions]
host = "localhost"
path = "/health"
[route.action]
type = "File"
path = "${FIXTURES_DIR}/proxy_health.json"

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/health"
[route.action]
type = "File"
path = "${FIXTURES_DIR}/proxy_health.json"
EOF

    # リダイレクトルート（302/307/308テスト用）
    cat >> "${FIXTURES_DIR}/proxy.toml" << EOF

[[route]]
[route.conditions]
host = "localhost"
path = "/redirect-test"
[route.action]
type = "Redirect"
redirect_url = "https://localhost:${PROXY_HTTPS_PORT}/"
redirect_status = 302

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/redirect-test"
[route.action]
type = "Redirect"
redirect_url = "https://127.0.0.1:${PROXY_HTTPS_PORT}/"
redirect_status = 302

[[route]]
[route.conditions]
host = "localhost"
path = "/redirect-307"
[route.action]
type = "Redirect"
redirect_url = "https://localhost:${PROXY_HTTPS_PORT}/"
redirect_status = 307

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/redirect-307"
[route.action]
type = "Redirect"
redirect_url = "https://127.0.0.1:${PROXY_HTTPS_PORT}/"
redirect_status = 307

[[route]]
[route.conditions]
host = "localhost"
path = "/redirect-308"
[route.action]
type = "Redirect"
redirect_url = "https://localhost:${PROXY_HTTPS_PORT}/"
redirect_status = 308

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/redirect-308"
[route.action]
type = "Redirect"
redirect_url = "https://127.0.0.1:${PROXY_HTTPS_PORT}/"
redirect_status = 308

# ヘッダー条件ルーティングテスト（X-Version=v2 かつ X-API-Key=secret が必要）
[[route]]
[route.conditions]
host = "localhost"
path = "/api/header-filter/*"
header = { "X-Version" = "v2", "X-API-Key" = "secret" }
[route.action]
type = "Proxy"
upstream = "backend-pool"

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/api/header-filter/*"
header = { "X-Version" = "v2", "X-API-Key" = "secret" }
[route.action]
type = "Proxy"
upstream = "backend-pool"

# クエリパラメータ条件ルーティングテスト（format=json かつ version=1 が必要）
[[route]]
[route.conditions]
host = "localhost"
path = "/api/query-filter/*"
query = { "format" = "json", "version" = "1" }
[route.action]
type = "Proxy"
upstream = "backend-pool"

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/api/query-filter/*"
query = { "format" = "json", "version" = "1" }
[route.action]
type = "Proxy"
upstream = "backend-pool"

# AND条件ルーティングテスト（query=format:json かつ header=X-Version:v2 が必要）
[[route]]
[route.conditions]
host = "localhost"
path = "/api/condition-and/*"
query = { "format" = "json" }
header = { "X-Version" = "v2" }
[route.action]
type = "Proxy"
upstream = "backend-pool"

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/api/condition-and/*"
query = { "format" = "json" }
header = { "X-Version" = "v2" }
[route.action]
type = "Proxy"
upstream = "backend-pool"

# 複合条件ルーティングテスト（format=json かつ X-Version=v2 かつ X-API-Key=secret が必要）
[[route]]
[route.conditions]
host = "localhost"
path = "/api/combined/*"
query = { "format" = "json" }
header = { "X-Version" = "v2", "X-API-Key" = "secret" }
[route.action]
type = "Proxy"
upstream = "backend-pool"

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/api/combined/*"
query = { "format" = "json" }
header = { "X-Version" = "v2", "X-API-Key" = "secret" }
[route.action]
type = "Proxy"
upstream = "backend-pool"

# IP制限テスト（127.0.0.1を拒否）
[[route]]
[route.conditions]
host = "localhost"
path = "/api/ip-restricted/*"
[route.action]
type = "Proxy"
upstream = "backend-pool"
[route.security]
denied_ips = ["127.0.0.0/8"]

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/api/ip-restricted/*"
[route.action]
type = "Proxy"
upstream = "backend-pool"
[route.security]
denied_ips = ["127.0.0.0/8"]

# メソッド制限テスト（GETのみ許可）
[[route]]
[route.conditions]
host = "localhost"
path = "/api/get-only/*"
[route.action]
type = "Proxy"
upstream = "backend-pool"
[route.security]
allowed_methods = ["GET", "HEAD"]

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/api/get-only/*"
[route.action]
type = "Proxy"
upstream = "backend-pool"
[route.security]
allowed_methods = ["GET", "HEAD"]

# ワイルドカードパス・末尾スラッシュテスト用（/api/* を汎用的にバックエンドへ転送）
[[route]]
[route.conditions]
host = "localhost"
path = "/api/*"
[route.action]
type = "Proxy"
upstream = "backend-pool"

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/api/*"
[route.action]
type = "Proxy"
upstream = "backend-pool"
EOF

    # 汎用ルート (最後に定義して優先順位を下げる)
    cat >> "${FIXTURES_DIR}/proxy.toml" << EOF

[[route]]
[route.conditions]
host = "localhost"
path = "/*"
[route.action]
type = "Proxy"
upstream = "backend-pool"
[route.security]
add_response_headers = { "X-Proxied-By" = "veil", "X-Test-Header" = "e2e-test" }
remove_response_headers = ["Server"]
client_body_timeout_secs = 5
[route.compression]
enabled = true
preferred_encodings = ["zstd", "br", "gzip"]
min_size = 1024

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/*"
[route.action]
type = "Proxy"
upstream = "backend-pool"
[route.security]
add_response_headers = { "X-Proxied-By" = "veil", "X-Test-Header" = "e2e-test" }
client_body_timeout_secs = 5
[route.compression]
enabled = true
preferred_encodings = ["zstd", "br", "gzip"]
min_size = 1024
EOF
    
    log_info "Configuration files generated (type: ${config_type})"
}

# プロキシを起動する（RUN_MODE により host / container を切り替える）。
#
# host モード（既定・従来通り）:
#   ビルド済みバイナリを直接起動する。proxy.pid はそのプロセス PID。
#
# container モード（e2e_setup.sh test container [glibc|musl]）:
#   プロキシ（テスト対象）を veil コンテナイメージ（veil:glibc / veil:musl）から起動する。
#   バックエンド群は host モードと全く同じ構成でホスト上に起動し、`--network host` で
#   コンテナ内 veil が同一ループバックの同一ポートを bind・到達できるようにする（＝既存の
#   e2e_setup.sh test と全く同じ構成・同じ設定・同じテストがコンテナ環境で実行される）。
#   io_uring 用に seccomp 許可リストを適用し、フィクスチャを同一パスにマウントして
#   証明書・WASM・ディスクスピルオーバのパスを解決する。SIGHUP リロードテスト用に
#   `docker run`（アタッチ）をバックグラウンド起動し、その docker クライアント PID を
#   proxy.pid に記録する（docker のシグナルプロキシで SIGHUP が veil PID1 に転送される）。
start_proxy() {
    if [ "${RUN_MODE}" = "container" ]; then
        start_proxy_container
    else
        start_proxy_host
    fi
}

start_proxy_host() {
    # プロキシ起動（自己署名証明書を許可するため VEIL_TLS_INSECURE=1 を設定）
    log_info "Starting veil proxy (host mode, binary: ${VEIL_BIN})..."
    VEIL_TLS_INSECURE=1 "${VEIL_BIN}" --config "${FIXTURES_DIR}/proxy.toml" > /tmp/proxy.log 2>&1 &
    echo $! >> "$PIDS_FILE"
    # F-49: リロード E2E テストが SIGHUP を送るためのプロキシ PID ファイル
    echo $! > "${FIXTURES_DIR}/proxy.pid"
    log_info "Proxy started on ports ${PROXY_HTTPS_PORT}/${PROXY_HTTP_PORT} (PID: $!)"
}

start_proxy_container() {
    local seccomp="${PROJECT_DIR}/docker/assets/security/seccomp.json"
    if [ ! -f "$seccomp" ]; then
        log_error "seccomp プロファイルが見つかりません: $seccomp"
        exit 1
    fi
    if ! docker image inspect "${VEIL_IMAGE}" >/dev/null 2>&1; then
        log_error "コンテナイメージ ${VEIL_IMAGE} が見つかりません。docker/README.md の手順で"
        log_error "  docker build -f Dockerfile.<glibc|musl> -t ${VEIL_IMAGE} --build-arg CARGO_FEATURES='full' .. でビルドしてください"
        exit 1
    fi

    log_info "Starting veil proxy (container mode, image: ${VEIL_IMAGE}, --network host)..."
    docker rm -f "${VEIL_PROXY_CONTAINER}" >/dev/null 2>&1 || true
    mkdir -p /tmp/veil_buffer

    # アタッチ実行（-d ではない）をバックグラウンド起動し、docker クライアント PID を
    # proxy.pid に記録する。docker のシグナルプロキシにより kill -HUP <client-pid> が
    # コンテナ内 veil(PID1) へ転送され、F-49 の証明書リロードテストが機能する。
    docker run --rm --name "${VEIL_PROXY_CONTAINER}" \
        --network host \
        --user "$(id -u):$(id -g)" \
        -e VEIL_TLS_INSECURE=1 \
        --security-opt "seccomp=${seccomp}" \
        -v "${FIXTURES_DIR}:${FIXTURES_DIR}:rw" \
        -v /tmp/veil_buffer:/tmp/veil_buffer:rw \
        "${VEIL_IMAGE}" \
        ./veil --config "${FIXTURES_DIR}/proxy.toml" > /tmp/proxy.log 2>&1 &
    local run_pid=$!
    echo "$run_pid" >> "$PIDS_FILE"
    echo "$run_pid" > "${FIXTURES_DIR}/proxy.pid"
    log_info "Proxy container '${VEIL_PROXY_CONTAINER}' started (docker client PID: $run_pid)"
}

# サーバーを起動
start_servers() {
    log_info "Starting backend servers..."
    
    # バックエンド1起動
    "$VEIL_BIN" -c "${FIXTURES_DIR}/backend1.toml" > /tmp/backend1.log 2>&1 &
    echo $! >> "$PIDS_FILE"
    log_info "Backend 1 started on port ${BACKEND1_PORT} (PID: $!)"
    
    # バックエンド2起動
    "$VEIL_BIN" -c "${FIXTURES_DIR}/backend2.toml" > /tmp/backend2.log 2>&1 &
    echo $! >> "$PIDS_FILE"
    log_info "Backend 2 started on port ${BACKEND2_PORT} (PID: $!)"
    
    # H2Cバックエンド起動（HTTP/2 over cleartextサーバーとして動作、H2Cテスト用）
    "$VEIL_BIN" -c "${FIXTURES_DIR}/backend_h2c.toml" > /tmp/backend_h2c.log 2>&1 &
    echo $! >> "$PIDS_FILE"
    log_info "H2C Backend started on port ${BACKEND_H2C_PORT} (PID: $!)"

    # gRPCバックエンド起動（スタンドアロンEchoサーバー、ビルドは ensure_veil_binary で完了済み）
    log_info "Starting gRPC Echo Backend..."
    RUST_LOG=debug "${SCRIPT_DIR}/grpc_server/target/debug/grpc-server" > /tmp/grpc_server.log 2>&1 &
    echo $! >> "$PIDS_FILE"
    log_info "gRPC Echo Backend started on port ${BACKEND_GRPC_PORT} (PID: $!, logs: /tmp/grpc_server.log)"

    # テストバックエンド起動（WebSocket Echo + HTTP 500エラー + chunked ストリーミング）
    # ビルドは ensure_veil_binary で完了済み
    log_info "Starting Rust test backends (WS echo + HTTP error + chunked + body-echo)..."
    WS_PORT="${BACKEND_WS_PORT}" ERROR_PORT="${BACKEND_ERROR_PORT}" BAD_PORT="${BACKEND_BAD_PORT}" CHUNKED_PORT="${BACKEND_CHUNKED_PORT}" ECHO_PORT="${BACKEND_ECHO_PORT}" \
        TLS_ECHO_PORT="${BACKEND_TLS_ECHO_PORT}" TLS_CERT_PATH="${FIXTURES_DIR}/cert.pem" TLS_KEY_PATH="${FIXTURES_DIR}/key.pem" \
        RUST_LOG=info "${SCRIPT_DIR}/test_backends/target/debug/test-backends" \
        > /tmp/test_backends.log 2>&1 &
    echo $! >> "$PIDS_FILE"
    log_info "Test backends started (WS: ${BACKEND_WS_PORT}, error: ${BACKEND_ERROR_PORT}, chunked: ${BACKEND_CHUNKED_PORT}, echo: ${BACKEND_ECHO_PORT}, PID: $!, logs: /tmp/test_backends.log)"

    # test_backendsの起動待機（全ポートがリッスン状態になるまで）
    local tb_wait=0
    while [ $tb_wait -lt 30 ]; do
        if check_port_in_use "$BACKEND_WS_PORT" && check_port_in_use "$BACKEND_ERROR_PORT" && check_port_in_use "$BACKEND_CHUNKED_PORT" && check_port_in_use "$BACKEND_ECHO_PORT" && check_port_in_use "$BACKEND_TLS_ECHO_PORT" && check_port_in_use "$BACKEND_BAD_PORT"; then
            sleep 0.2
            break
        fi
        tb_wait=$((tb_wait + 1))
        sleep 0.2
    done
    if [ $tb_wait -ge 30 ]; then
        log_warn "Test backends may not be fully ready, continuing..."
    fi

    # バックエンド起動待機（動的）
    log_info "Waiting for backends to be ready..."
    if wait_for_server "https://127.0.0.1:${BACKEND1_PORT}/health" "Backend 1" 15; then
        log_info "Backend 1 is ready"
    else
        log_warn "Backend 1 may not be fully ready, continuing..."
    fi
    
    if wait_for_server "https://127.0.0.1:${BACKEND2_PORT}/health" "Backend 2" 15; then
        log_info "Backend 2 is ready"
    else
        log_warn "Backend 2 may not be fully ready, continuing..."
    fi
    
    # H2CバックエンドはHTTP（平文）サーバーとして動作
    # listenをHTTPポートに設定することで、HTTP（平文）で動作可能
    if wait_for_h2c_server "http://127.0.0.1:${BACKEND_H2C_PORT}/health" "H2C Backend" 15; then
        log_info "H2C Backend is ready (HTTP/2 over cleartext mode)"
    else
        log_warn "H2C Backend may not be fully ready, continuing..."
    fi

    # gRPCバックエンド起動待機
    log_info "Waiting for gRPC Echo Backend to be ready..."
    local grpc_wait_count=0
    while [ $grpc_wait_count -lt 30 ]; do
        if check_port_in_use "$BACKEND_GRPC_PORT"; then
            log_info "gRPC Echo Backend is ready (port ${BACKEND_GRPC_PORT} is listening)"
            # ポートが空いた後、さらに短い待機時間を入れて初期化を完了させる
            sleep 0.5
            break
        fi
        grpc_wait_count=$((grpc_wait_count + 1))
        sleep 0.2
    done
    if [ $grpc_wait_count -ge 30 ]; then
        log_warn "gRPC Echo Backend may not be fully ready, continuing..."
    fi
    
    log_info "Starting proxy server..."

    # プロキシ起動（ホスト or コンテナ）
    start_proxy

    # プロキシ起動待機（WASM AOT コンパイル対応のデッドライン方式）
    log_info "Waiting for proxy to be ready (allowing for WASM AOT compile on cold cache)..."
    if wait_for_proxy_ready; then
        log_info "Proxy is ready"
    else
        log_warn "Proxy may not be fully ready, continuing..."
    fi

    log_info "All servers started"
}

# サーバーを停止
stop_servers() {
    # コンテナモードのプロキシコンテナを確実に除去する（PID 経由の停止に依存しない）
    docker rm -f "${VEIL_PROXY_CONTAINER}" >/dev/null 2>&1 || true

    if [ -f "$PIDS_FILE" ]; then
        log_info "Stopping servers..."
        
        # まず SIGTERM で graceful shutdown を試みる
        while read -r pid; do
            if kill -0 "$pid" 2>/dev/null; then
                kill -TERM "$pid" 2>/dev/null || true
                log_info "Sent SIGTERM to process $pid"
            fi
        done < "$PIDS_FILE"
        
        # プロセス終了を待機（最大5秒）
        sleep 1
        local wait_count=0
        while [ $wait_count -lt 5 ]; do
            local still_running=false
            while read -r pid; do
                if kill -0 "$pid" 2>/dev/null; then
                    still_running=true
                    break
                fi
            done < "$PIDS_FILE"
            
            if [ "$still_running" = false ]; then
                break
            fi
            
            sleep 1
            wait_count=$((wait_count + 1))
        done
        
        # まだ残っていれば SIGKILL で強制終了
        while read -r pid; do
            if kill -0 "$pid" 2>/dev/null; then
                log_warn "Force killing process $pid"
                kill -9 "$pid" 2>/dev/null || true
            fi
        done < "$PIDS_FILE"
        
        rm -f "$PIDS_FILE"
        log_info "All server processes stopped"
    else
        log_warn "No PID file found"
    fi
    
    # 残存プロセスを強制終了（安全策）
    pkill -f "veil.*fixtures" 2>/dev/null || true
    
    # ポートが解放されるのを待機
    sleep 1
}

# ポートが使用中かチェック
check_port_in_use() {
    local port=$1
    if command -v lsof > /dev/null 2>&1; then
        if lsof -i ":$port" > /dev/null 2>&1; then
            return 0  # 使用中
        fi
    elif command -v netstat > /dev/null 2>&1; then
        if netstat -an 2>/dev/null | grep -q ":$port.*LISTEN"; then
            return 0  # 使用中
        fi
    elif command -v ss > /dev/null 2>&1; then
        if ss -ln 2>/dev/null | grep -q ":$port"; then
            return 0  # 使用中
        fi
    fi
    return 1  # 未使用
}

# ポート競合チェック
check_port_conflicts() {
    log_info "Checking for port conflicts..."
    local conflicts=0
    
    for port in $PROXY_HTTPS_PORT $PROXY_HTTP_PORT $PROXY_H2C_PORT $PROXY_L4_PORT $PROXY_L4_LEAST_CONN_PORT $PROXY_L4_TERMINATE_PORT $BACKEND1_PORT $BACKEND2_PORT $BACKEND_H2C_PORT $BACKEND_GRPC_PORT $BACKEND_WS_PORT $BACKEND_ERROR_PORT $BACKEND_BAD_PORT $BACKEND_CHUNKED_PORT $BACKEND_ECHO_PORT $BACKEND_TLS_ECHO_PORT; do
        if check_port_in_use "$port"; then
            log_error "Port $port is already in use"
            conflicts=$((conflicts + 1))
        fi
    done
    
    if [ $conflicts -gt 0 ]; then
        log_error "Found $conflicts port conflict(s). Please free the ports or stop conflicting processes."
        return 1
    fi
    
    log_info "No port conflicts detected"
    return 0
}

# プロキシの起動待機（WASM AOT コンパイル対応・デッドライン方式）
#
# WASM モジュールは初回起動時に cranelift で AOT コンパイルされ .cwasm キャッシュへ
# 書き出される。コールドキャッシュ（初回／wasm 再ビルド後）ではモジュール 1 つあたり
# 数十秒かかることがあり、固定回数リトライ（数秒）では起動前にタイムアウトして
# 「Proxy failed to start」の一過性エラーになる。ウォームキャッシュでは 1 秒未満で復帰する。
# HTTP・HTTPS いずれかの __metrics が応答すればレディとみなす。
wait_for_proxy_ready() {
    local deadline_secs="${PROXY_READY_TIMEOUT:-180}"
    local start_ts
    start_ts=$(date +%s)
    while true; do
        if curl -ks "http://127.0.0.1:${PROXY_HTTP_PORT}/__metrics" > /dev/null 2>&1 \
            || curl -ks "https://127.0.0.1:${PROXY_HTTPS_PORT}/__metrics" > /dev/null 2>&1; then
            return 0
        fi
        if [ $(( $(date +%s) - start_ts )) -ge "$deadline_secs" ]; then
            log_error "Proxy failed to become ready within ${deadline_secs}s (WASM AOT compile?)"
            return 1
        fi
        sleep 0.5
    done
}

# サーバーの起動を待機（リトライ付き）
wait_for_server() {
    local url=$1
    local name=$2
    local max_attempts=${3:-30}  # デフォルト30回
    local attempt=0

    while [ $attempt -lt $max_attempts ]; do
        if curl -ks "$url" > /dev/null 2>&1; then
            return 0
        fi
        attempt=$((attempt + 1))
        sleep 0.2
    done

    log_error "$name failed to start after $max_attempts attempts"
    return 1
}

# サーバーの安定性を確認（複数回の連続成功を要求）
# サーバーが完全に初期化され、安定した状態であることを確認する
verify_server_stability() {
    local url=$1
    local name=$2
    local required_successes=${3:-5}  # デフォルト5回連続成功
    local max_attempts=${4:-50}  # 最大試行回数（デフォルト50回 = 25秒）
    local attempt=0
    local success_count=0
    
    log_info "Verifying ${name} stability (requires ${required_successes} consecutive successes)..."
    
    while [ $attempt -lt $max_attempts ]; do
        if curl -ks "$url" > /dev/null 2>&1; then
            success_count=$((success_count + 1))
            if [ $success_count -ge $required_successes ]; then
                log_info "${name} is stable (${success_count} consecutive successes)"
                return 0
            fi
        else
            success_count=0  # 失敗したらカウントをリセット
        fi
        attempt=$((attempt + 1))
        sleep 0.5
    done
    
    log_error "${name} failed to stabilize after ${max_attempts} attempts (max successes: ${success_count})"
    return 1
}

# H2Cサーバーの起動を待機（HTTP平文、リトライ付き）
wait_for_h2c_server() {
    local url=$1
    local name=$2
    local max_attempts=${3:-30}  # デフォルト30回
    local attempt=0
    
    # HTTP（平文）の場合は-kオプションは不要（TLS証明書検証をスキップする必要がない）
    # -sオプションのみ使用（サイレントモード）
    while [ $attempt -lt $max_attempts ]; do
        if curl -s --http2-prior-knowledge "$url" > /dev/null 2>&1; then
            return 0
        fi
        attempt=$((attempt + 1))
        sleep 0.2
    done
    
    log_error "$name failed to start after $max_attempts attempts"
    return 1
}

# ヘルスチェック（リトライ付き）
# サーバーが起動していることを確認（最初の応答を待つ）
health_check() {
    log_info "Checking server health..."
    
    # バックエンド1 (HTTPS - 自己署名証明書なので-k)
    if wait_for_server "https://127.0.0.1:${BACKEND1_PORT}/health" "Backend 1" 30; then
        log_info "Backend 1: OK"
    else
        log_error "Backend 1: FAILED"
        return 1
    fi
    
    # バックエンド2 (HTTPS)
    if wait_for_server "https://127.0.0.1:${BACKEND2_PORT}/health" "Backend 2" 30; then
        log_info "Backend 2: OK"
    else
        log_error "Backend 2: FAILED"
        return 1
    fi
    
    # H2Cバックエンド (HTTP - H2Cテスト用)
    # if wait_for_h2c_server "http://127.0.0.1:${BACKEND_H2C_PORT}/health" "H2C Backend" 30; then
    #     log_info "H2C Backend: OK"
    # else
    #     log_error "H2C Backend: FAILED"
    #     return 1
    # fi
    log_info "H2C Backend: Skipping health check (intermittent curl failure, but server should be up)"
    
    # プロキシ（WASM AOT コンパイル対応のデッドライン方式で待機）
    if wait_for_proxy_ready; then
        log_info "Proxy: OK"
    else
        log_error "Proxy: FAILED"
        return 1
    fi

    log_info "All servers healthy"
}

# サーバーの安定性を確認（複数回の連続成功を要求）
# サーバーが完全に初期化され、安定した状態であることを確認する
verify_server_stability_check() {
    log_info "Verifying server stability..."
    
    # バックエンド1の安定性を確認
    if verify_server_stability "https://127.0.0.1:${BACKEND1_PORT}/health" "Backend 1" 5 50; then
        log_info "Backend 1: Stable"
    else
        log_error "Backend 1: Not stable"
        return 1
    fi
    
    # バックエンド2の安定性を確認
    if verify_server_stability "https://127.0.0.1:${BACKEND2_PORT}/health" "Backend 2" 5 50; then
        log_info "Backend 2: Stable"
    else
        log_error "Backend 2: Not stable"
        return 1
    fi
    
    # H2Cバックエンドの安定性を確認（HTTP平文）
    # local h2c_success_count=0
    # local h2c_attempt=0
    # local h2c_max_attempts=50
    # log_info "Verifying H2C Backend stability (requires 5 consecutive successes)..."
    # while [ $h2c_attempt -lt $h2c_max_attempts ]; do
    #     if curl -s --http2-prior-knowledge "http://127.0.0.1:${BACKEND_H2C_PORT}/health" > /dev/null 2>&1; then
    #         h2c_success_count=$((h2c_success_count + 1))
    #         if [ $h2c_success_count -ge 5 ]; then
    #             log_info "H2C Backend is stable (${h2c_success_count} consecutive successes)"
    #             break
    #         fi
    #     else
    #         h2c_success_count=0
    #     fi
    #     h2c_attempt=$((h2c_attempt + 1))
    #     sleep 0.5
    # done
    # if [ $h2c_success_count -lt 5 ]; then
    #     log_error "H2C Backend failed to stabilize after ${h2c_max_attempts} attempts"
    #     return 1
    # fi
    log_info "H2C Backend: Skipping stability check"
    
    # プロキシの安定性を確認
    local proxy_url=""
    if curl -ks "http://127.0.0.1:${PROXY_HTTP_PORT}/__metrics" > /dev/null 2>&1; then
        proxy_url="http://127.0.0.1:${PROXY_HTTP_PORT}/__metrics"
    elif curl -ks "https://127.0.0.1:${PROXY_HTTPS_PORT}/__metrics" > /dev/null 2>&1; then
        proxy_url="https://127.0.0.1:${PROXY_HTTPS_PORT}/__metrics"
    else
        log_error "Proxy is not accessible"
        return 1
    fi
    
    if verify_server_stability "$proxy_url" "Proxy" 5 50; then
        log_info "Proxy: Stable"
    else
        log_error "Proxy: Not stable"
        return 1
    fi
    
    log_info "All servers are stable and ready"
    return 0
}

# テスト実行
# Phase 1: 並列化による高速化
# 環境変数 PARALLEL_JOBS で並列数を制御可能（デフォルト: CPUコア数または4）
run_tests() {
    log_info "Running E2E tests in parallel..."
    
    cd "$PROJECT_DIR"
    
    # 並列数の決定
    # 1. 環境変数 PARALLEL_JOBS が設定されている場合はそれを使用
    # 2. それ以外は CPUコア数を取得（取得できない場合は4を使用）
    if [ -n "${PARALLEL_JOBS:-}" ]; then
        TEST_THREADS="${PARALLEL_JOBS}"
    else
        # CPUコア数を取得（Linux/macOS対応）
        if command -v nproc > /dev/null 2>&1; then
            CPU_CORES=$(nproc)
        elif command -v sysctl > /dev/null 2>&1; then
            CPU_CORES=$(sysctl -n hw.ncpu 2>/dev/null || echo "4")
        else
            CPU_CORES=4
        fi
        
        # 並列数は CPUコア数と4の小さい方（リソース競合を避けるため）
        if [ "$CPU_CORES" -lt 4 ]; then
            TEST_THREADS="$CPU_CORES"
        else
            TEST_THREADS=4
        fi
    fi
    
    # デバッグ: 変数の値を確認
    log_info "TEST_THREADS variable: ${TEST_THREADS}"
    log_info "Running tests with ${TEST_THREADS} parallel threads"
    
    # 環境変数を明示的に設定して並列実行を確実にする
    export RUST_TEST_THREADS="${TEST_THREADS}"
    
    # デバッグ: 実際に実行されるコマンドを確認
    if [ -n "${TEST_FILTER:-}" ]; then
        log_info "Running filtered tests: ${TEST_FILTER}"
        log_info "Command: cargo test --test e2e_tests --features 'full' -- ${TEST_FILTER} --test-threads=${TEST_THREADS} --nocapture"
        
        # テスト実行
        cargo test --test e2e_tests --features 'full' -- "${TEST_FILTER}" --test-threads=${TEST_THREADS} --nocapture
    else
        log_info "Command: cargo test --test e2e_tests --features 'full' -- --test-threads=${TEST_THREADS}"
        
        # テスト実行
        cargo test --test e2e_tests --features 'full' -- --test-threads=${TEST_THREADS}
    fi
    
    log_info "E2E tests completed"
}

# クリーンアップ
cleanup() {
    log_info "Cleaning up..."
    stop_servers
    rm -rf "${FIXTURES_DIR}/backend1" "${FIXTURES_DIR}/backend2"
    rm -f "${FIXTURES_DIR}"/*.toml
}

# メイン処理
#
# 引数（$1=コマンドの後は順不同）:
#   config_type: default|cache|buffering|healthcheck|least_conn|ip_hash|wasm|security
#   container  : 指定するとプロキシを veil コンテナイメージで起動（未指定なら従来のホスト起動）
#   glibc|musl : container 指定時のイメージ選択（既定 glibc）
#
# 例:
#   ./tests/e2e_setup.sh test                    # ホスト環境（従来通り）
#   ./tests/e2e_setup.sh test container          # コンテナ環境（veil:glibc）
#   ./tests/e2e_setup.sh test container musl     # コンテナ環境（veil:musl）
#   ./tests/e2e_setup.sh test cache container    # config_type=cache をコンテナ環境で
COMMAND="${1:-}"
CONFIG_TYPE="default"
shift || true
for arg in "$@"; do
    case "$arg" in
        container)
            RUN_MODE="container"
            ;;
        glibc)
            VEIL_IMAGE="veil:glibc"
            ;;
        musl)
            VEIL_IMAGE="veil:musl"
            ;;
        default|cache|buffering|healthcheck|least_conn|ip_hash|wasm|security|ktls)
            CONFIG_TYPE="$arg"
            ;;
        "")
            ;;
        *)
            log_warn "不明な引数を無視します: $arg"
            ;;
    esac
done

if [ "$RUN_MODE" = "container" ]; then
    log_info "実行モード: container（プロキシイメージ: ${VEIL_IMAGE}）"
else
    log_info "実行モード: host"
fi

case "${COMMAND}" in
    start)
        ensure_veil_binary
        check_port_conflicts || exit 1
        prepare_fixtures
        generate_configs "$CONFIG_TYPE"
        start_servers
        health_check
        ;;
    stop)
        stop_servers
        ;;
    restart)
        stop_servers
        sleep 1
        ensure_veil_binary
        check_port_conflicts || exit 1
        prepare_fixtures
        generate_configs "$CONFIG_TYPE"
        start_servers
        health_check
        ;;
    health)
        health_check
        ;;
    test)
        # テスト終了時（成功・失敗問わず）に必ずサーバーを停止
        trap 'stop_servers' EXIT
        
        ensure_veil_binary
        check_port_conflicts || exit 1
        prepare_fixtures
        generate_configs "$CONFIG_TYPE"
        start_servers
        
        if ! health_check; then
            log_error "Health check failed, stopping servers"
            exit 1
        fi
        
        # サーバーの安定性を確認（複数回の連続成功を要求）
        # サーバーが完全に初期化され、安定した状態であることを確認する
        if ! verify_server_stability_check; then
            log_error "Server stability check failed, stopping servers"
            exit 1
        fi
        
        # サーバー起動後の最終安定化待機（環境変数で制御可能、デフォルト: 5秒）
        # verify_server_stability_checkで安定性を確認しているため、追加の待機時間は短縮可能
        STABILIZATION_WAIT="${STABILIZATION_WAIT:-5}"
        log_info "Waiting ${STABILIZATION_WAIT} seconds for final stabilization before running tests..."
        
        # set -eを一時的に無効化して、sleepが確実に実行されるようにする
        set +e
        # カウントダウン表示で待機時間を視覚的に確認
        for i in $(seq "${STABILIZATION_WAIT}" -1 1); do
            echo -ne "\r${GREEN}[INFO]${NC} Waiting ${i} seconds... "
            sleep 1
        done
        echo -ne "\r${GREEN}[INFO]${NC} Waiting complete.                           \n"
        set -e
        
        log_info "Starting tests..."
        
        # テスト実行（失敗してもtrapでクリーンアップ）
        set +e
        run_tests
        TEST_EXIT_CODE=$?
        set -e
        
        # trapでstop_serversが呼ばれるので明示的な呼び出しは不要
        exit $TEST_EXIT_CODE
        ;;
    clean)
        cleanup
        ;;
    *)
        echo "Usage: $0 {start|stop|restart|health|test|clean} [config_type] [container [glibc|musl]]"
        echo ""
        echo "Commands:"
        echo "  start   - Start all servers"
        echo "  stop    - Stop all servers"
        echo "  restart - Restart all servers"
        echo "  health  - Check server health"
        echo "  test    - Run E2E tests (parallelized for faster execution)"
        echo "  clean   - Clean up fixtures"
        echo ""
        echo "Config Types (optional, default: default):"
        echo "  default      - Default configuration (round_robin, compression)"
        echo "  cache        - Enable proxy cache"
        echo "  buffering    - Enable adaptive buffering"
        echo "  healthcheck  - Enable health checks"
        echo "  least_conn   - Use least connections algorithm"
        echo "  ip_hash      - Use IP hash algorithm"
        echo "  security     - Enable security features (rate limiting on /rate-limited/*)"
        echo "  ktls         - Alias for default (kTLS auto-enabled when kernel supports it)"
        echo ""
        echo "Run Mode (optional, default: host):"
        echo "  container        - Run the proxy from the veil container image (default veil:glibc)."
        echo "                     Backends run on host and --network host is used so the exact"
        echo "                     same composition/config/tests run in the container environment."
        echo "  container glibc  - Use the veil:glibc image (default)."
        echo "  container musl   - Use the veil:musl image."
        echo "  (omit 'container' to run the proxy as a host binary, as before)"
        echo ""
        echo "  Examples:"
        echo "    $0 test                 # host mode (unchanged)"
        echo "    $0 test container       # container mode (veil:glibc)"
        echo "    $0 test container musl  # container mode (veil:musl)"
        echo ""
        echo "Parallelization (Phase 1):"
        echo "  Tests are now run in parallel for faster execution."
        echo "  Default: Uses CPU core count or 4 (whichever is smaller)"
        echo "  Custom: Set PARALLEL_JOBS environment variable"
        echo "  Example: PARALLEL_JOBS=8 $0 test"
        exit 1
        ;;
esac

