# HTTP/2, HTTP/3 & WebSocket

[← Documentation index](README.md) · [日本語](ja/protocols.md)

## HTTP/2 Support

Supports HTTP/2 (RFC 7540) via TLS ALPN negotiation.

### Features

| Feature | Effect |
|---------|--------|
| Stream Multiplexing | Parallel processing of multiple requests on a single connection |
| HPACK Header Compression | Significantly reduces header overhead. Decode uses a **4-bit LUT state machine** (F-121; 256 states × 16 peeks, 16 KiB L1-resident packed table) instead of scanning the encode table per symbol — release microbench ~12× vs the prior linear decoder; EOS padding and B-21 invalid-input safety retained |
| Server Push | Latency reduction through proactive resource sending |
| Flow Control | Stream and connection level control |

### Enabling

```bash
# Build with HTTP/2 feature
cargo build --release --features http2
```

```toml
# config.toml
[server]
listen = "0.0.0.0:443"
http2_enabled = true  # Enable HTTP/2 (ALPN h2)
```

### Advanced Configuration

Configure detailed HTTP/2 protocol parameters in the `[http2]` section:

```toml
[http2]
# HPACK dynamic table size (default: 65536)
header_table_size = 65536

# Concurrent streams (default: 256)
max_concurrent_streams = 256

# Stream window size (default: 1048576 = 1MB)
initial_window_size = 1048576

# Maximum frame size (default: 65536)
max_frame_size = 65536

# Maximum header list size (default: 65536)
max_header_list_size = 65536

# Connection window size (default: 1048576 = 1MB)
connection_window_size = 1048576
```

### DoS Protection

HTTP/2 DoS attack mitigations are enabled by default. Configure in the `[http2]` section:

| Attack | CVE | Setting | Default |
|--------|-----|---------|---------|
| Rapid Reset | CVE-2023-44487 | `max_rst_stream_per_second` | 100 |
| CONTINUATION Flood | CVE-2024-24786 | `max_continuation_frames` | 10 |
| Control Frame Flood | - | `max_control_frames_per_second` | 500 |
| HPACK Bomb | - | `max_header_block_size` | 65536 |
| Slow Loris | - | `stream_idle_timeout_secs` | 60 |

```toml
[http2]
# RST_STREAM rate limit (per second)
# Rapid Reset attack mitigation (CVE-2023-44487)
max_rst_stream_per_second = 100

# Control frame rate limit (per second)
# Mitigates PING/SETTINGS flood attacks
max_control_frames_per_second = 500

# CONTINUATION frame limit (per header block)
# CONTINUATION Flood mitigation (CVE-2024-24786)
max_continuation_frames = 10

# Maximum header block size (bytes)
# HPACK Bomb mitigation
max_header_block_size = 65536

# Stream idle timeout (seconds)
# Slow Loris mitigation (0 = disabled)
stream_idle_timeout_secs = 60
```

When limits are exceeded, the server responds with `ENHANCE_YOUR_CALM` (0xb) error and closes the connection.

### HTTP/1.1 Fallback

Clients that don't support HTTP/2 automatically fall back to HTTP/1.1.

## HTTP/3 Support

Supports HTTP/3 (RFC 9114) based on QUIC/UDP. Uses Cloudflare's [quiche](https://github.com/cloudflare/quiche).

### Features

| Feature | Effect |
|---------|--------|
| 0-RTT Connection Establishment | Instant communication without TLS handshake |
| Head-of-Line Blocking Elimination | Packet loss doesn't affect other streams |
| Connection Migration | Maintains connection during network switches |
| GSO/GRO Optimization | High-performance UDP processing (UDP_SEGMENT / UDP_GRO) |
| Congestion control / pacing | Configurable via `[http3]` (`cc_algorithm`, `pacing`, `hystart`, …); default BBR + pacing |
| io_uring pipelined UDP I/O | Receive/send hot path uses pipelined `IORING_OP_RECVMSG`/`IORING_OP_SENDMSG` (batch via `mmsg_batch_size`, default 64); no libc `recvmmsg`/`sendmmsg` on the hot path (F-130) |

### Enabling

```bash
# Build with HTTP/3 feature
cargo build --release --features http3
```

```toml
# config.toml
[server]
listen = "0.0.0.0:443"
http3_enabled = true  # Enable HTTP/3 (QUIC/UDP)
```

### Advanced Configuration

Configure detailed HTTP/3 (QUIC) protocol parameters in the `[http3]` section:

```toml
[http3]
# HTTP/3 listen address (UDP, defaults to server.listen if unspecified)
listen = "0.0.0.0:443"

# Maximum idle timeout (milliseconds, default: 30000)
max_idle_timeout = 30000

# Maximum UDP payload size (default: 1350)
max_udp_payload_size = 1350

# Initial maximum data size (entire connection, default: 10000000)
initial_max_data = 10000000

# Initial maximum stream data size (bidirectional local, default: 1000000)
initial_max_stream_data_bidi_local = 1000000

# Initial maximum stream data size (bidirectional remote, default: 1000000)
initial_max_stream_data_bidi_remote = 1000000

# Initial maximum stream data size (unidirectional, default: 1000000)
initial_max_stream_data_uni = 1000000

# Initial maximum bidirectional streams (default: 100)
initial_max_streams_bidi = 100

# Initial maximum unidirectional streams (default: 100)
initial_max_streams_uni = 100

# GSO/GRO optimization (UDP performance optimization)
# GSO (Generic Segmentation Offload) / GRO (Generic Receive Offload) are
# kernel-level features that optimize UDP packet transmission and reception.
#
# Effects:
#   - Send (GSO): coalesce same-destination/same-size QUIC packets into one
#     sendmsg(UDP_SEGMENT) call
#   - Receive (GRO): coalesce multiple datagrams of the same flow in one recvmsg
#   - Reduce system call overhead and CPU usage
#   - The HTTP/3 receive loop reuses a single buffer and feeds GRO segments to
#     quiche as slices, eliminating per-datagram heap allocation and copies
#     (zero-copy receive). Falls back to single-datagram I/O on unsupported kernels.
#   - Independent of this setting, the HTTP/3 data plane always batches multiple
#     datagrams (across different connections) via pipelined io_uring ops on the
#     default backend: `mmsg_batch_size` IORING_OP_RECVMSG ops kept in flight for
#     receive, and multiple IORING_OP_SENDMSG SQEs per io_uring_enter for send
#     (F-130; batch size = mmsg_batch_size, default 64). Falls back to a single
#     recvmmsg(2)/sendmmsg(2) syscall per sweep (F-115) on reactor builds or when
#     the io_uring pipeline is disabled (VEIL_H3_MULTISHOT=0).
#     Container deployments need recvmmsg/sendmmsg in the seccomp allowlist for
#     this fallback path and for DNS resolution
#     (docker/assets/security/seccomp.json ships with them).
#
# Notes:
#   - Supported on Linux 5.0+
#   - May not work as expected in some virtual environments or Docker
#   - Set to false if issues occur
#
# Default: false
gso_gro_enabled = false

# Alt-Svc (HTTP/3 advertisement on HTTP/1.1 and HTTP/2 responses; F-94)
# All Alt-Svc keys live under [http3]. Effective only when server.http3_enabled = true.
alt_svc_enabled = true          # default: true; set false to suppress advertising
# alt_svc = "h3=\":443\"; ma=86400"  # optional full override (auto from listen port if omitted)
# alt_svc_ma_secs = 86400            # max-age for auto-generated value (default: 86400)
```

### Notes

- HTTP/3 is UDP-based, so **kTLS cannot be used** (doesn't use TCP)
- UDP port 443 must be opened in the firewall
- When `server.http3_enabled = true`, HTTP/1.1 and HTTP/2 responses advertise HTTP/3 via `Alt-Svc` (all options under `[http3]`: `alt_svc_enabled`, optional `alt_svc` / `alt_svc_ma_secs`)

## WebSocket Support

Supports WebSocket (RFC 6455) proxying.
Automatically detects `Connection: Upgrade` and `Upgrade: websocket` headers
and performs bidirectional data transfer.

### Behavior

1. Detect Upgrade request from client
2. Forward Upgrade request to backend
3. Receive 101 Switching Protocols
4. Start bidirectional bypass transfer (operates in configured polling mode)
5. Continue until either connection closes

### Polling Modes

Control polling behavior during WebSocket bidirectional transfer via configuration.

| Mode | Description | Use Case |
|------|-------------|----------|
| `adaptive` (default) | Short during data transfer, longer when idle | General purpose, CPU efficiency focused |
| `fixed` | Always uses fixed timeout | Real-time games, low latency priority |

See the "[WebSocket Configuration](routing.md#websocket-configuration)" section for detailed configuration options.

### Configuration Examples

WebSocket is automatically supported with regular Proxy backends:

```toml
# WebSocket application (default settings)
[[route]]
[route.conditions]
host = "example.com"
path = "/ws/*"
[route.action]
type = "Proxy"
url = "http://localhost:3000"

# Low latency configuration (for real-time games)
[[route]]
[route.conditions]
host = "game.example.com"
path = "/ws/*"
[route.action]
type = "Proxy"
url = "http://localhost:3001"

[route.security]
  websocket_poll_mode = "fixed"
  websocket_poll_timeout_ms = 1
```

### Supported Backends

| Protocol | Support |
|----------|---------|
| HTTP → WS | ✅ |
| HTTPS → WSS | ✅ |
