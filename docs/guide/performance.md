# Performance Tuning & Benchmarking

[← Documentation index](README.md) · [日本語](ja/performance.md)

## Performance Tuning

### Worker Thread Count

Configure worker thread count in the `[server]` section of `config.toml`.

```toml
[server]
listen = "0.0.0.0:443"
threads = 0  # If unspecified or 0, uses same number as CPU cores
```

| Setting | Behavior |
|---------|----------|
| Unspecified | Same number of threads as CPU cores |
| `threads = 0` | Same number of threads as CPU cores |
| `threads = 4` | Start with 4 threads |

- Each worker thread is pinned to a CPU core (CPU affinity)
- If thread count exceeds core count, assigned round-robin
- Recommend setting lower in memory-constrained environments

### SO_REUSEPORT CBPF Load Balancing

#### Overview

When multiple worker threads listen on the same port using SO_REUSEPORT, the Linux kernel distributes connections by default using a 3-tuple hash (protocol + source IP + source port). In CBPF mode, a custom BPF program is attached to the kernel that selects a worker based on the flow hash (`skb->hash`, computed from the 4-tuple: source/destination IP and port), falling back to the receiving CPU when the hash is not yet computed. This is an option for pinning connections from many clients to a fixed worker; it is not a default recommendation — the default remains `kernel`.

#### Effects

| Aspect | Kernel (default) | CBPF |
|--------|------------------|------|
| Distribution Key | protocol + src IP + src port | flow hash (4-tuple), falls back to receiving CPU |
| Same Connection | Fixed for the connection's lifetime | Always same worker |
| CPU Cache Efficiency | Medium | Comparable (both keep a connection on one worker) |
| TLS Session Resumption | Medium | Comparable |

> **Note (B-76):** the kernel default already hashes the 4-tuple, so in practice `"cbpf"`
> now behaves very close to `"kernel"`. The mode exists so that the selection policy is
> expressed by a program you control (and so it can fall back to the receiving CPU when
> `skb->hash` is unavailable). Before B-76 this mode was broken: the program always
> returned index 0, so **every connection landed on worker 0** and multi-worker
> parallelism was lost entirely.

#### Configuration

```toml
[performance]
# "kernel" = kernel default [default]
# "cbpf"   = flow hash-based CBPF (4-tuple; pins each connection to a fixed worker)
reuseport_balancing = "cbpf"
```

#### Requirements

- **Linux 4.6 or higher** (SO_ATTACH_REUSEPORT_CBPF support)
- Automatically falls back to kernel default if CBPF attach fails

### Huge Pages (Large OS Pages)

#### Overview

Using Huge Pages (2MB) with the mimalloc allocator reduces TLB (Translation Lookaside Buffer) misses and improves performance.

#### Effects

| Aspect | Effect |
|--------|--------|
| TLB Misses | Significantly reduced (fewer page table lookups) |
| Page Faults | Reduced when using large amounts of memory |
| Performance | 5-10% improvement (workload dependent) |
| kTLS/splice | Especially effective with kernel integration |

#### Configuration

```toml
[performance]
huge_pages_enabled = true
```

#### OS-Level Configuration (Linux)

```bash
# Temporarily enable Huge Pages (128 pages = 256MB)
echo 128 | sudo tee /proc/sys/vm/nr_hugepages

# Persist (/etc/sysctl.conf)
echo "vm.nr_hugepages=128" | sudo tee -a /etc/sysctl.conf
sudo sysctl -p

# Check current Huge Pages status
grep -i huge /proc/meminfo
```

#### Container Environment Notes

In Docker/Kubernetes environments, Huge Pages must be reserved on the host side:

```bash
# Reserve Huge Pages on host
echo 128 | sudo tee /proc/sys/vm/nr_hugepages

# When starting Docker (optional)
docker run --shm-size=256m ...

# Kubernetes (add to Pod spec)
# resources.limits.hugepages-2Mi: "256Mi"
```

If Huge Pages are unavailable, automatically falls back to regular 4KB pages.

### System Configuration

At startup, veil automatically raises the soft limit of `RLIMIT_NOFILE` up to the hard limit (equivalent to nginx's `worker_rlimit_nofile`). Control the hard limit via systemd's `LimitNOFILE` or docker's `--ulimit nofile` to manage the effective file descriptor ceiling.

```bash
# File descriptor limit
ulimit -n 65535

# Kernel parameters
sysctl -w net.core.somaxconn=65535
sysctl -w net.ipv4.tcp_max_syn_backlog=65535
sysctl -w net.core.netdev_max_backlog=65535

# io_uring settings (as needed)
sysctl -w kernel.io_uring_setup_flags=0
```

### Buffer Sizes and Timeouts

Constants in code (set at compile time, requires rebuild):

```rust
// Buffer sizes
const BUF_SIZE: usize = 65536;           // 64KB - optimal size for io_uring
const HEADER_BUF_CAPACITY: usize = 512;  // For HTTP headers
const MAX_HEADER_SIZE: usize = 8192;     // 8KB - header size limit
const MAX_BODY_SIZE: usize = 10485760;   // 10MB - body size limit

// Timeouts
const READ_TIMEOUT: Duration = Duration::from_secs(30);   // Read timeout
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);  // Write timeout
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10); // Backend connection timeout
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);   // Keep-Alive idle timeout
```

> **Note**: Some timeouts can be individually adjusted from config.toml via per-route security settings using `client_header_timeout_secs` and `backend_connect_timeout_secs`.

### Buffer Pool Configuration

The buffer pool reduces memory allocation overhead by pre-allocating buffers at startup. Configure in the `[buffer_pool]` section:

```toml
[buffer_pool]
# Read buffer size (bytes)
# Default: 65536 (64KB)
read_buffer_size = 65536

# Initial number of read buffers in pool
# Default: 32
initial_read_buffers = 32

# Maximum number of read buffers in pool
# Default: 128
max_read_buffers = 128

# Request construction buffer size (bytes)
# Default: 1024 (1KB)
request_buffer_size = 1024

# Initial number of request buffers in pool
# Default: 16
initial_request_buffers = 16

# Large request buffer size (bytes)
# Default: 4096 (4KB)
large_request_buffer_size = 4096

# Path string buffer size (bytes)
# Default: 256

# Response header buffer size (bytes)
# Default: 512
```

**Note**: Buffer pool configuration is optional. Default values are optimized for most use cases. Adjust only if you have specific memory constraints or performance requirements.

## Benchmarking

Veil ships a reproducible Docker-based performance harness in
[`tools/perf/`](../../tools/perf/) that compares `veil:glibc` / `veil:musl` (built with
**full features**) against `nginx:alpine` over container-to-container networking, across
HTTP/1.1 (`wrk`), HTTP/2 (`h2load`), **HTTP/3 (QUIC-enabled `h2load`)**, and
**gRPC / WebSocket (`grafana/k6`)**. It covers the default+http2 tuning matrix
(http2 × kTLS × SO_REUSEPORT balancing × open_file_cache) **and** per-feature showcase
configs that layer a single full-only feature on a shared baseline — compression / cache /
buffering / reverse-proxy / **wasm / metrics / access-log / rate-limit / admin /
opentelemetry / l4-proxy / http3 / grpc / websocket** (`h2_1_feat_*` / `h2_0_feat_l4`).
It also generates a **full protocol × feature matrix** (`h2_1_proxy_*` / `h3_file_*` /
`h3_proxy*` / `grpc_h2_*` / `grpc_h3*`) layering each feature onto Proxy / HTTP/3 / gRPC
(over H2/H3) routes; scope a run with `CONFIG_GLOB` (e.g. `CONFIG_GLOB='h3_*'`). gRPC over
HTTP/3 is failsafe-skipped (`NA`) as k6 lacks a native client.
The HTTP/3 client needs a QUIC-enabled h2load (`docker build -t local/h2load-h3:latest
tools/perf/h2load-http3`; http3 is skipped if absent); gRPC/WebSocket use `grafana/k6`
against `moul/grpcbin` / `jmalloc/echo-server` upstreams. Full data and a summary are in
[**docs/perf**](../perf/); the summary and raw TSV are in
[docs/perf/README.md](../perf/README.md) / [docs/perf/results_raw.tsv](../perf/results_raw.tsv)
(TLS termination is the dominant cost — plaintext L4 is up to 2.2× faster — while L7 feature
logic stays within noise, all configs Non-2xx=0).
Latest results (2026-07-16 full-suite for v0.5.0, all 105 config×proto rows Non-2xx=0,
[docs/perf/README.md](../perf/README.md)):
HTTP/1.1 3213 req/s (nginx ×1.47) / HTTP/2 2763 req/s (nginx ×1.18; 3646 req/s with a
multi-threaded h2load — HTTP/2 now beats HTTP/1.1 thanks to the F-116 stream multiplexing) /
**HTTP/3 835 req/s (doubled by F-115 recvmmsg/sendmmsg batching + the B-43 StreamBlocked
fix; ~906 req/s as a host-network reference)** / gRPC relay 1609 req/s (a k6→grpcbin control
run shows the proxy hop adds effectively zero overhead) / plaintext L4 passthrough 5074 req/s.
The v0.5.0 full-suite run also uncovered and fixed B-44 (startup RLIMIT_NOFILE raise +
backend-connection churn), B-45 (L4 half-close propagation) and B-46 (HTTP/3 buffered-proxy
duplicate content-length).
Note: HTTP/3 mmsg batching requires `recvmmsg`/`sendmmsg` in the Docker seccomp allowlist
(`docker/assets/security/seccomp.json` ships with them).

```bash
# Generate configs, run the comparison, and aggregate (median±stdev)
bash tools/perf/gen_configs.sh
bash tools/perf/run_perf.sh
# Results: tools/perf/results/results_raw.tsv and results_summary.md
```

Ad-hoc single-target benchmarking:

```bash
# Benchmark using wrk
wrk -t4 -c100 -d30s https://localhost/

# Comparison with kTLS enabled/disabled

# 1. kTLS disabled (using rustls)
cargo build --release
./veil -c ./examples/config.toml &
wrk -t4 -c100 -d30s https://localhost/

# 2. kTLS enabled (using rustls + custom kTLS module)
cargo build --release --features ktls
# Set ktls_enabled = true in config.toml
./veil -c ./examples/config.toml &
wrk -t4 -c100 -d30s https://localhost/
```
