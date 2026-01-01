#!/bin/bash
# E2Eテスト環境セットアップスクリプト
#
# veilをバックエンドサーバーとしても使用する構成:
#   - プロキシ: veil (ポート8443/8080)
#   - バックエンド1: veil (ポート9001、静的ファイル配信)
#   - バックエンド2: veil (ポート9002、静的ファイル配信)
#
# 環境適応型フィーチャー選択:
#   - kTLSが利用可能な場合: ktls,http2,http3 を使用（推奨構成）
#   - kTLSが利用不可の場合: http2,http3 にフォールバック
#
# 使用方法:
#   ./tests/e2e_setup.sh start   # 環境起動
#   ./tests/e2e_setup.sh stop    # 環境停止
#   ./tests/e2e_setup.sh test    # テスト実行

set -e

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

# ポート設定
PROXY_HTTPS_PORT=8443
PROXY_HTTP_PORT=8080
BACKEND1_PORT=9001
BACKEND2_PORT=9002
BACKEND_H2C_PORT=9003

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

# ビルド用フィーチャーを決定
# kTLSが利用可能な場合は "ktls,http2,http3"、利用不可の場合は "http2,http3" を返す
determine_build_features() {
    if check_ktls_available; then
        echo "ktls,http2,http3"
    else
        echo "http2,http3"
    fi
}

# veilバイナリの存在確認・ビルド
# 注意: kTLSが利用可能な場合は推奨構成（ktls,http2,http3）でビルドします
# kTLS対応には Linux 5.15+ と modprobe tls が必要です
ensure_veil_binary() {
    if [ ! -f "$VEIL_BIN" ]; then
        FEATURES=$(determine_build_features)
        if check_ktls_available; then
            log_info "Building veil with recommended features (ktls,http2,http3)..."
        else
            log_warn "kTLS not available, falling back to http2,http3"
        fi
        cd "$PROJECT_DIR"
        cargo build --features "$FEATURES"
        cd - > /dev/null
    fi
    
    if [ ! -f "$VEIL_BIN" ]; then
        log_error "Failed to build veil binary"
        exit 1
    fi
    
    log_info "Using veil binary: $VEIL_BIN"
}

# フィクスチャディレクトリの準備
prepare_fixtures() {
    mkdir -p "$FIXTURES_DIR"
    mkdir -p "${FIXTURES_DIR}/backend1"
    mkdir -p "${FIXTURES_DIR}/backend2"
    
    # テスト用証明書を生成
    # CA:FALSE を指定して end-entity 証明書として生成
    # （CA 証明書として生成すると CaUsedAsEndEntity エラーになる）
    if [ ! -f "${FIXTURES_DIR}/cert.pem" ]; then
        log_info "Generating test certificates..."
        
        # OpenSSLで自己署名証明書を生成（end-entity証明書）
        openssl req -x509 -newkey rsa:2048 -nodes \
            -keyout "${FIXTURES_DIR}/key.pem" \
            -out "${FIXTURES_DIR}/cert.pem" \
            -days 365 \
            -subj "/CN=localhost" \
            -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" \
            -addext "basicConstraints=critical,CA:FALSE" \
            -addext "keyUsage=critical,digitalSignature,keyEncipherment" \
            -addext "extendedKeyUsage=serverAuth" \
            2>/dev/null
        
        log_info "Certificates generated"
    fi
    
    # バックエンド1用テストファイル
    echo "Hello from Backend 1" > "${FIXTURES_DIR}/backend1/index.html"
    echo '{"server": "backend1", "status": "ok"}' > "${FIXTURES_DIR}/backend1/health"
    echo "Large content from backend1: $(head -c 10000 /dev/urandom | base64)" > "${FIXTURES_DIR}/backend1/large.txt"
    
    # バックエンド2用テストファイル  
    echo "Hello from Backend 2" > "${FIXTURES_DIR}/backend2/index.html"
    echo '{"server": "backend2", "status": "ok"}' > "${FIXTURES_DIR}/backend2/health"
    echo "Large content from backend2: $(head -c 10000 /dev/urandom | base64)" > "${FIXTURES_DIR}/backend2/large.txt"
    
    # H2Cバックエンド用テストファイル
    mkdir -p "${FIXTURES_DIR}/backend_h2c"
    echo "Hello from H2C Backend" > "${FIXTURES_DIR}/backend_h2c/index.html"
    echo '{"server": "backend_h2c", "status": "ok"}' > "${FIXTURES_DIR}/backend_h2c/health"
    echo "H2C test content" > "${FIXTURES_DIR}/backend_h2c/test.txt"
}

# 設定ファイルを生成
# 引数: 設定タイプ (default|cache|buffering|healthcheck|least_conn|ip_hash)
generate_configs() {
    local config_type="${1:-default}"
    
    # バックエンド1設定（静的ファイル配信）
    cat > "${FIXTURES_DIR}/backend1.toml" << EOF
[server]
listen = "127.0.0.1:${BACKEND1_PORT}"
threads = 1

[tls]
cert_path = "${FIXTURES_DIR}/cert.pem"
key_path = "${FIXTURES_DIR}/key.pem"

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
    # 注意: veil-proxyは現在H2Cサーバーとして動作しない可能性があるため、
    # この設定はHTTP/1.1サーバーとして動作します
    # H2Cテストでは、プロキシがH2C接続を試みることを確認します
    # TLSセクションは必須ですが、実際にはHTTP/1.1サーバーとして動作します
    cat > "${FIXTURES_DIR}/backend_h2c.toml" << EOF
[server]
listen = "127.0.0.1:${BACKEND_H2C_PORT}"
threads = 1

[tls]
cert_path = "${FIXTURES_DIR}/cert.pem"
key_path = "${FIXTURES_DIR}/key.pem"

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
redirect_http_to_https = false
threads = 1
http3_enabled = true

[tls]
cert_path = "${FIXTURES_DIR}/cert.pem"
key_path = "${FIXTURES_DIR}/key.pem"

[logging]
level = "debug"

[prometheus]
enabled = true
path = "/__metrics"

[http3]
listen = "127.0.0.1:${PROXY_HTTPS_PORT}"
compression_enabled = true

[upstreams."backend-pool"]
algorithm = "${algorithm}"
servers = [
    "https://127.0.0.1:${BACKEND1_PORT}",
    "https://127.0.0.1:${BACKEND2_PORT}"
]
tls_insecure = true
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
    
    # ルート設定
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
[route.compression]
enabled = true
preferred_encodings = ["zstd", "br", "gzip"]
min_size = 1024
EOF

    # キャッシュ設定を追加（デフォルトでも有効化、cacheタイプの場合は詳細設定）
    if [ "$config_type" = "cache" ] || [ "$config_type" = "default" ]; then
        cat >> "${FIXTURES_DIR}/proxy.toml" << EOF
[route.cache]
enabled = true
max_memory_size = 10485760
default_ttl_secs = 60
methods = ["GET", "HEAD"]
cacheable_statuses = [200, 301, 302, 304]
stale_while_revalidate = true
stale_if_error = true
respect_vary = true
enable_etag = true
EOF
    fi
    
    # バッファリング設定を追加（デフォルトでも有効化、bufferingタイプの場合は詳細設定）
    if [ "$config_type" = "buffering" ] || [ "$config_type" = "default" ]; then
        cat >> "${FIXTURES_DIR}/proxy.toml" << EOF
[route.buffering]
mode = "adaptive"
max_memory_buffer = 10485760
adaptive_threshold = 1048576
EOF
    fi
    
    # バッファリングモード別のルート設定（streaming, full, adaptive）
    if [ "$config_type" = "buffering" ] || [ "$config_type" = "default" ]; then
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
EOF
    fi
    
    # セキュリティ設定を追加（レート制限、IP制限）
    if [ "$config_type" = "security" ]; then
        cat >> "${FIXTURES_DIR}/proxy.toml" << EOF
[route.security]
rate_limit_requests_per_min = 30
allowed_ips = ["127.0.0.1", "::1"]
EOF
    fi
    
    # 2つ目のルート設定
    cat >> "${FIXTURES_DIR}/proxy.toml" << EOF

[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/*"
[route.action]
type = "Proxy"
upstream = "backend-pool"
[route.security]
add_response_headers = { "X-Proxied-By" = "veil" }

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
EOF

    log_info "Configuration files generated (type: ${config_type})"
}

# サーバーを起動
start_servers() {
    log_info "Starting backend servers..."
    
    # バックエンド1起動
    "$VEIL_BIN" -c "${FIXTURES_DIR}/backend1.toml" &
    echo $! >> "$PIDS_FILE"
    log_info "Backend 1 started on port ${BACKEND1_PORT} (PID: $!)"
    
    # バックエンド2起動
    "$VEIL_BIN" -c "${FIXTURES_DIR}/backend2.toml" &
    echo $! >> "$PIDS_FILE"
    log_info "Backend 2 started on port ${BACKEND2_PORT} (PID: $!)"
    
    # H2Cバックエンド起動（HTTP/1.1サーバーとして動作、H2Cテスト用）
    "$VEIL_BIN" -c "${FIXTURES_DIR}/backend_h2c.toml" &
    echo $! >> "$PIDS_FILE"
    log_info "H2C Backend started on port ${BACKEND_H2C_PORT} (PID: $!)"
    
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
    
    # H2CバックエンドはHTTPSサーバーとして動作（H2CテストではプロキシがH2C接続を試みる）
    # 注意: バックエンドサーバーはHTTPSとして動作するが、プロキシ設定でuse_h2c=trueを指定しているため、
    # プロキシはH2C（平文HTTP/2）接続を試みる。これはエラーハンドリングのテストとして機能する。
    if wait_for_server "https://127.0.0.1:${BACKEND_H2C_PORT}/health" "H2C Backend" 15; then
        log_info "H2C Backend is ready (HTTPS mode, H2C connection will be attempted by proxy)"
    else
        log_warn "H2C Backend may not be fully ready, continuing..."
    fi
    
    log_info "Starting proxy server..."
    
    # プロキシ起動（自己署名証明書を許可するためVEIL_TLS_INSECURE=1を設定）
    VEIL_TLS_INSECURE=1 "$VEIL_BIN" -c "${FIXTURES_DIR}/proxy.toml" &
    echo $! >> "$PIDS_FILE"
    log_info "Proxy started on ports ${PROXY_HTTPS_PORT}/${PROXY_HTTP_PORT} (PID: $!)"
    
    # プロキシ起動待機（動的）
    log_info "Waiting for proxy to be ready..."
    if wait_for_server "http://127.0.0.1:${PROXY_HTTP_PORT}/__metrics" "Proxy" 15; then
        log_info "Proxy is ready"
    else
        if wait_for_server "https://127.0.0.1:${PROXY_HTTPS_PORT}/__metrics" "Proxy HTTPS" 15; then
            log_info "Proxy HTTPS is ready"
        else
            log_warn "Proxy may not be fully ready, continuing..."
        fi
    fi
    
    log_info "All servers started"
}

# サーバーを停止
stop_servers() {
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
    
    for port in $PROXY_HTTPS_PORT $PROXY_HTTP_PORT $BACKEND1_PORT $BACKEND2_PORT $BACKEND_H2C_PORT; do
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

# ヘルスチェック（リトライ付き）
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
    
    # プロキシ（HTTPポート - リダイレクト無効なのでHTTPで接続可能）
    if wait_for_server "http://127.0.0.1:${PROXY_HTTP_PORT}/__metrics" "Proxy HTTP" 30; then
        log_info "Proxy HTTP: OK"
    else
        # HTTPSでも確認
        if wait_for_server "https://127.0.0.1:${PROXY_HTTPS_PORT}/__metrics" "Proxy HTTPS" 30; then
            log_info "Proxy HTTPS: OK"
        else
            log_error "Proxy: FAILED"
            return 1
        fi
    fi
    
    log_info "All servers healthy"
}

# テスト実行
run_tests() {
    log_info "Running E2E tests..."
    
    cd "$PROJECT_DIR"
    cargo test --test e2e_tests -- --test-threads=1
    
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
CONFIG_TYPE="${2:-default}"  # 設定タイプ（default|cache|buffering|healthcheck|least_conn|ip_hash）

case "${1:-}" in
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
        echo "Usage: $0 {start|stop|restart|health|test|clean} [config_type]"
        echo ""
        echo "Commands:"
        echo "  start   - Start all servers"
        echo "  stop    - Stop all servers"
        echo "  restart - Restart all servers"
        echo "  health  - Check server health"
        echo "  test    - Run E2E tests"
        echo "  clean   - Clean up fixtures"
        echo ""
        echo "Config Types (optional, default: default):"
        echo "  default      - Default configuration (round_robin, compression)"
        echo "  cache        - Enable proxy cache"
        echo "  buffering    - Enable adaptive buffering"
        echo "  healthcheck  - Enable health checks"
        echo "  least_conn   - Use least connections algorithm"
        echo "  ip_hash      - Use IP hash algorithm"
        echo "  security     - Enable security features (rate limiting, IP restriction)"
        exit 1
        ;;
esac

