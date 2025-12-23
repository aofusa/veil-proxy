#!/bin/bash
# E2Eテスト環境セットアップスクリプト
#
# veilをバックエンドサーバーとしても使用する構成:
#   - プロキシ: veil (ポート8443/8080)
#   - バックエンド1: veil (ポート9001、静的ファイル配信)
#   - バックエンド2: veil (ポート9002、静的ファイル配信)
#
# 使用方法:
#   ./tests/e2e_setup.sh start   # 環境起動
#   ./tests/e2e_setup.sh stop    # 環境停止
#   ./tests/e2e_setup.sh test    # テスト実行

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"
VEIL_BIN="${PROJECT_DIR}/target/debug/veil"
FIXTURES_DIR="${SCRIPT_DIR}/fixtures"
PIDS_FILE="${FIXTURES_DIR}/pids.txt"

# ポート設定
PROXY_HTTPS_PORT=8443
PROXY_HTTP_PORT=8080
BACKEND1_PORT=9001
BACKEND2_PORT=9002

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

# veilバイナリの存在確認・ビルド
ensure_veil_binary() {
    if [ ! -f "$VEIL_BIN" ]; then
        log_info "Building veil..."
        cd "$PROJECT_DIR"
        cargo build --features http2
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
    
    # テスト用証明書を生成（veilのtest機能を使用）
    if [ ! -f "${FIXTURES_DIR}/cert.pem" ]; then
        log_info "Generating test certificates..."
        
        # OpenSSLで自己署名証明書を生成
        openssl req -x509 -newkey rsa:2048 -nodes \
            -keyout "${FIXTURES_DIR}/key.pem" \
            -out "${FIXTURES_DIR}/cert.pem" \
            -days 365 \
            -subj "/CN=localhost" \
            -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" \
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
}

# 設定ファイルを生成
generate_configs() {
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

[path_routes."localhost"."/"]
type = "File"
path = "${FIXTURES_DIR}/backend1"
index = "index.html"

[path_routes."localhost"."/".security]
add_response_headers = { "X-Server-Id" = "backend1" }

[path_routes."127.0.0.1"."/"]
type = "File"
path = "${FIXTURES_DIR}/backend1"
index = "index.html"

[path_routes."127.0.0.1"."/".security]
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

[path_routes."localhost"."/"]
type = "File"
path = "${FIXTURES_DIR}/backend2"
index = "index.html"

[path_routes."localhost"."/".security]
add_response_headers = { "X-Server-Id" = "backend2" }

[path_routes."127.0.0.1"."/"]
type = "File"
path = "${FIXTURES_DIR}/backend2"
index = "index.html"

[path_routes."127.0.0.1"."/".security]
add_response_headers = { "X-Server-Id" = "backend2" }
EOF

    # プロキシ設定（ロードバランシング）
    cat > "${FIXTURES_DIR}/proxy.toml" << EOF
[server]
listen = "127.0.0.1:${PROXY_HTTPS_PORT}"
http_listen = "127.0.0.1:${PROXY_HTTP_PORT}"
redirect_http_to_https = true
threads = 1

[tls]
cert_path = "${FIXTURES_DIR}/cert.pem"
key_path = "${FIXTURES_DIR}/key.pem"

[logging]
level = "debug"

[prometheus]
enabled = true
path = "/__metrics"

[upstreams."backend-pool"]
algorithm = "round_robin"
servers = [
    "https://127.0.0.1:${BACKEND1_PORT}",
    "https://127.0.0.1:${BACKEND2_PORT}"
]

[upstreams."backend-pool".health_check]
enabled = true
path = "/health"
interval_secs = 5
timeout_secs = 2

[path_routes."localhost"."/"]
type = "Upstream"
upstream = "backend-pool"

[path_routes."localhost"."/".security]
add_response_headers = { "X-Proxied-By" = "veil", "X-Test-Header" = "e2e-test" }
remove_response_headers = ["Server"]

[path_routes."localhost"."/".compression]
enabled = true
preferred_encodings = ["zstd", "br", "gzip"]
min_size = 1024

[path_routes."127.0.0.1"."/"]
type = "Upstream"
upstream = "backend-pool"

[path_routes."127.0.0.1"."/".security]
add_response_headers = { "X-Proxied-By" = "veil" }
EOF

    log_info "Configuration files generated"
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
    
    # バックエンド起動待機
    sleep 1
    
    log_info "Starting proxy server..."
    
    # プロキシ起動
    "$VEIL_BIN" -c "${FIXTURES_DIR}/proxy.toml" &
    echo $! >> "$PIDS_FILE"
    log_info "Proxy started on ports ${PROXY_HTTPS_PORT}/${PROXY_HTTP_PORT} (PID: $!)"
    
    # 起動完了待機
    sleep 2
    
    log_info "All servers started"
}

# サーバーを停止
stop_servers() {
    if [ -f "$PIDS_FILE" ]; then
        log_info "Stopping servers..."
        while read -r pid; do
            if kill -0 "$pid" 2>/dev/null; then
                kill "$pid" 2>/dev/null || true
                log_info "Stopped process $pid"
            fi
        done < "$PIDS_FILE"
        rm -f "$PIDS_FILE"
    else
        log_warn "No PID file found"
    fi
    
    # 残存プロセスを強制終了
    pkill -f "veil.*fixtures" 2>/dev/null || true
}

# ヘルスチェック
health_check() {
    log_info "Checking server health..."
    
    # バックエンド1
    if curl -ks "https://127.0.0.1:${BACKEND1_PORT}/health" > /dev/null 2>&1; then
        log_info "Backend 1: OK"
    else
        log_error "Backend 1: FAILED"
        return 1
    fi
    
    # バックエンド2
    if curl -ks "https://127.0.0.1:${BACKEND2_PORT}/health" > /dev/null 2>&1; then
        log_info "Backend 2: OK"
    else
        log_error "Backend 2: FAILED"
        return 1
    fi
    
    # プロキシ（メトリクスエンドポイント）
    if curl -ks "https://127.0.0.1:${PROXY_HTTPS_PORT}/__metrics" > /dev/null 2>&1; then
        log_info "Proxy: OK"
    else
        log_error "Proxy: FAILED"
        return 1
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
case "${1:-}" in
    start)
        ensure_veil_binary
        prepare_fixtures
        generate_configs
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
        prepare_fixtures
        generate_configs
        start_servers
        health_check
        ;;
    health)
        health_check
        ;;
    test)
        ensure_veil_binary
        prepare_fixtures
        generate_configs
        start_servers
        sleep 2
        health_check
        run_tests
        stop_servers
        ;;
    clean)
        cleanup
        ;;
    *)
        echo "Usage: $0 {start|stop|restart|health|test|clean}"
        echo ""
        echo "Commands:"
        echo "  start   - Start all servers"
        echo "  stop    - Stop all servers"
        echo "  restart - Restart all servers"
        echo "  health  - Check server health"
        echo "  test    - Run E2E tests"
        echo "  clean   - Clean up fixtures"
        exit 1
        ;;
esac

