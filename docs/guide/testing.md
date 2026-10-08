# Testing (Developers)

[← Documentation index](README.md) · [日本語](ja/testing.md)

## Testing

Veil includes comprehensive test suites covering unit tests, integration tests, and end-to-end (E2E) tests.

### Test Overview

| Test Type | Count | Status |
|-----------|-------|--------|
| **Unit Tests** (lib) | 694 | ✅ All passing |
| **Integration Tests** (`integration_tests` + property + cancellation-safety) | 55+ | ✅ All passing |
| **E2E Tests** (`e2e_tests`) | 419 | ✅ All passing |
| **Fuzz Targets** (`cargo fuzz`) | 8 | ✅ No crashes |
| **Benchmarks** | 13 files | ✅ Ready |

The fuzz targets include `io_uring_executor`, which injects arbitrary pseudo-CQE sequences
into the runtime's completion-dispatch path (`src/runtime/executor.rs`) to assert the op-table
never panics, leaks slots, or runs a drop guard more than once — complementing the
`runtime_cancellation_test` integration test that randomly drops in-flight recv/send/accept/timer
futures on a live ring.

Hot-path discipline (the "no blocking calls on the event loop" rule) is enforced at the AST level
by clippy `disallowed-methods` in [`clippy.toml`](../../clippy.toml): synchronous `std::fs`,
`std::thread::sleep`, and blocking `std::net` sockets are rejected in data-plane code, with
reason-annotated `#[allow]` at the legitimate call sites (offload closures, dedicated threads,
startup/reload cold paths, tests/benches).

The unit-test count is verified inside the release image build (`docker/Dockerfile.musl`
runs `cargo test --lib --features full` — 883 passed). In addition to the in-repo tests,
two Docker-based external-verification harnesses are provided:

- **[`tools/container_security/`](../../tools/container_security/)** — fuzzing, chaos, h2spec HTTP/2
  conformance, request-smuggling / differential probes, and image/security scanners, run
  against the **full-features** container image. Fuzzing and the malformed-backend mock are
  Rust binaries (no Python); TLS/plaintext probes use `openssl` / bash `/dev/tcp`.
- **[`tools/perf/`](../../tools/perf/)** — `veil:glibc` / `veil:musl` (full features) vs `nginx`
  throughput/latency/CPU/memory comparison, covering both default+http2 tuning and the
  full-only features (compression / cache / buffering / reverse-proxy). See [docs/perf](../perf/).

### Running Tests

#### Unit Tests

```bash
# Run all unit tests
cargo test --features full --bin veil

# Run specific test module
cargo test --features full --bin veil wasm::tests

# Run with output
cargo test --features full --bin veil -- --nocapture
```

#### Integration Tests

```bash
# Run integration tests
cargo test --test integration_tests --features full
```

#### E2E Tests

E2E tests require a running test environment. Use the setup script:

```bash
# Method 1: Automated (recommended) — proxy runs as a host binary
./tests/e2e_setup.sh test

# Method 2: Manual
./tests/e2e_setup.sh start
cargo test --test e2e_tests --features full -- --test-threads=1
./tests/e2e_setup.sh stop

# Cleanup only
./tests/e2e_setup.sh clean
```

**Container mode** — run the exact same E2E suite/config/topology but with the proxy launched
from the **veil container image** (validates the shipped image). Pass `container` (and optionally
`glibc`/`musl`, default `glibc`). Backends run on the host and `--network host` keeps ports/config
identical; omit `container` for the traditional host-binary run.

```bash
# Container E2E with veil:glibc (default image)
./tests/e2e_setup.sh test container

# Container E2E with veil:musl
./tests/e2e_setup.sh test container musl
```

> The container images must be built with full features first
> (`docker build -f docker/Dockerfile.glibc -t veil:glibc --build-arg CARGO_FEATURES='full' .`).

#### Benchmarks

```bash
# Start E2E environment
./tests/e2e_setup.sh start

# Run all benchmarks
cargo bench --features full

# Run specific benchmark
cargo bench --bench throughput --features full
cargo bench --bench latency --features full

# WASM filter overhead (requires the proxy started with the WASM route, e.g. via e2e_setup).
# Compares an identical request through a WASM-filtered route (/wasm/*) vs a plain route (/);
# the keep-alive group amortizes connection cost to isolate the per-request filter overhead.
# Expected order: a few µs to tens of µs per request for a header filter (machine/wasmtime
# dependent). Measure RSS separately with `/usr/bin/time -v`.
cargo bench --bench wasm --features wasm

# Stop environment
./tests/e2e_setup.sh stop

# Or use automated script
./tests/run_bench.sh          # All benchmarks
./tests/run_bench.sh throughput  # Throughput only
./tests/run_bench.sh latency     # Latency only
```

### Test Coverage

#### Unit Tests (469 tests)

- **CIDR/IP Filtering**: IP address filtering, CIDR range validation
- **Rate Limiting**: Sliding window rate limiting, entry management
- **Configuration Parsing**: TOML parsing, default values
- **Load Balancing**: Round Robin, Least Connections, IP Hash algorithms
- **Health Checks**: Server state management, success/failure counting
- **Connection Pooling**: Pool management, timeout validation
- **Cache Management**: Memory/disk cache, key generation
- **HTTP/2**: Frame encoding/decoding, HPACK compression
- **Security**: Security configuration, kernel version detection
- **WASM**: Proxy-Wasm ABI, filter lifecycle, host function callbacks
- **Utilities**: Various helper functions

#### Integration Tests (12 tests)

- TCP connection handling
- HTTP server responses
- Multiple server coordination
- Dynamic port allocation
- TLS certificate generation
- Configuration file generation
- Port availability utilities

#### E2E Tests (23 tests)

- **Proxy Core**: Basic requests, health endpoints
- **Header Manipulation**: Add/remove headers, backend ID
- **Load Balancing**: Round Robin distribution
- **Static File Serving**: Index files, large files
- **Compression**: gzip, brotli, priority handling
- **Backend Access**: Direct backend connections
- **Prometheus**: Metrics endpoint
- **Error Handling**: 404 responses
- **HTTP Redirect**: HTTP to HTTPS redirect
- **Concurrency**: Concurrent and sequential requests
- **Performance**: Response time validation
- **Content Types**: HTML, JSON handling
- **Keep-Alive**: Persistent connections
- **Custom Headers**: User-Agent, Host headers

### Environment Cleanup

All test environments are automatically cleaned up:

- **Rust Drop Traits**: Server structs automatically terminate on scope exit
- **Shell Script Traps**: Cleanup on success, failure, or interruption
- **Graceful Shutdown**: SIGTERM → wait → SIGKILL staged termination
- **Process Cleanup**: Automatic cleanup of remaining processes

The cleanup mechanism ensures a clean state after test execution, regardless of test outcome.

### Test Files Structure

```
veil-proxy/
├── src/
│   ├── main.rs          # 103 unit tests
│   ├── security.rs      # 26 unit tests
│   ├── cache/           # 50+ unit tests
│   ├── http2/           # 30+ unit tests
│   └── ...
├── tests/
│   ├── integration_tests.rs  # 13 integration tests
│   ├── e2e_tests.rs          # 24 E2E tests
│   ├── e2e_setup.sh         # E2E environment setup
│   ├── run_bench.sh         # Benchmark automation
│   └── common/
│       └── mod.rs            # Test utilities
└── benches/
    ├── throughput.rs      # Throughput benchmarks
    ├── latency.rs         # Latency benchmarks
    ├── http2.rs           # HTTP/2 benchmarks
    ├── http3.rs           # HTTP/3 benchmarks
    ├── tls.rs             # TLS benchmarks
    ├── compression.rs     # Compression benchmarks
    ├── connection_pool.rs # Connection pool benchmarks
    ├── cache.rs           # Cache benchmarks
    ├── load_balancing.rs  # Load balancing benchmarks
    ├── websocket.rs       # WebSocket benchmarks
    ├── memory.rs          # Memory usage benchmarks
    └── routing.rs         # Routing benchmarks
```

### Continuous Integration

For CI/CD pipelines:

```yaml
# Example GitHub Actions workflow
- name: Run tests
  run: |
    cargo test --features http2 --all-targets
    
- name: Run E2E tests
  run: |
    ./tests/e2e_setup.sh test
```
