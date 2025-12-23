#!/bin/bash
# ベンチマーク実行スクリプト
#
# E2E環境を起動してベンチマークを実行し、終了後に環境をクリーンアップします。
#
# 使用方法:
#   ./tests/run_bench.sh                    # 全ベンチマーク実行
#   ./tests/run_bench.sh throughput         # スループットベンチマークのみ
#   ./tests/run_bench.sh latency            # レイテンシベンチマークのみ
#   ./tests/run_bench.sh http2              # HTTP/2ベンチマークのみ
#   ./tests/run_bench.sh compression        # 圧縮ベンチマークのみ
#   ./tests/run_bench.sh tls                # TLS/kTLSベンチマークのみ
#   ./tests/run_bench.sh connection_pool    # コネクションプールベンチマークのみ
#   ./tests/run_bench.sh cache             # キャッシュベンチマークのみ
#   ./tests/run_bench.sh load_balancing    # ロードバランシングベンチマークのみ
#   ./tests/run_bench.sh http3             # HTTP/3ベンチマークのみ（--features http3）
#   ./tests/run_bench.sh websocket         # WebSocketベンチマークのみ
#   ./tests/run_bench.sh memory            # メモリ使用量ベンチマークのみ

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

# 色付き出力
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
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

log_section() {
    echo -e "\n${BLUE}========================================${NC}"
    echo -e "${BLUE} $1${NC}"
    echo -e "${BLUE}========================================${NC}\n"
}

# クリーンアップ関数
cleanup() {
    log_info "Cleaning up..."
    "${SCRIPT_DIR}/e2e_setup.sh" stop 2>/dev/null || true
}

# 終了時に必ずクリーンアップ
trap cleanup EXIT

# ベンチマーク種別
BENCH_TYPE="${1:-all}"

log_section "Veil Proxy Benchmark Suite"
log_info "Benchmark type: ${BENCH_TYPE}"

# 1. E2E環境を起動
log_section "Starting E2E Environment"
"${SCRIPT_DIR}/e2e_setup.sh" start

# 起動確認
sleep 2
if ! "${SCRIPT_DIR}/e2e_setup.sh" health; then
    log_error "E2E environment failed to start"
    exit 1
fi

# 2. ベンチマーク実行
log_section "Running Benchmarks"

cd "$PROJECT_DIR"

case "$BENCH_TYPE" in
    throughput)
        log_info "Running throughput benchmark..."
        cargo bench --bench throughput --features http2
        ;;
    latency)
        log_info "Running latency benchmark..."
        cargo bench --bench latency --features http2
        ;;
    http2)
        log_info "Running HTTP/2 benchmark..."
        cargo bench --bench http2 --features http2
        ;;
    compression)
        log_info "Running compression benchmark..."
        cargo bench --bench compression --features http2
        ;;
    tls)
        log_info "Running TLS/kTLS benchmark..."
        cargo bench --bench tls --features http2
        ;;
    connection_pool)
        log_info "Running connection pool benchmark..."
        cargo bench --bench connection_pool --features http2
        ;;
    cache)
        log_info "Running cache benchmark..."
        cargo bench --bench cache --features http2
        ;;
    load_balancing)
        log_info "Running load balancing benchmark..."
        cargo bench --bench load_balancing --features http2
        ;;
    http3)
        log_info "Running HTTP/3 benchmark..."
        cargo bench --bench http3 --features http3
        ;;
    websocket)
        log_info "Running WebSocket benchmark..."
        cargo bench --bench websocket --features http2
        ;;
    memory)
        log_info "Running memory benchmark..."
        cargo bench --bench memory --features http2
        ;;
    all)
        log_info "Running all benchmarks..."
        cargo bench --features http2
        ;;
    *)
        log_error "Unknown benchmark type: $BENCH_TYPE"
        echo "Usage: $0 [throughput|latency|http2|compression|tls|connection_pool|cache|load_balancing|http3|websocket|memory|all]"
        exit 1
        ;;
esac

log_section "Benchmark Complete"
log_info "Results are available in: target/criterion/"

# trapでクリーンアップが行われる

