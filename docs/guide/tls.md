# TLS

[← Documentation index](README.md) · [日本語](ja/tls.md)

## TLS Certificate Generation

To generate a self-signed certificate for development/testing, run the following commands:

```bash
# Generate ECDSA private key (secp384r1)
openssl genpkey -algorithm EC -out server.key -pkeyopt ec_paramgen_curve:secp384r1 -pkeyopt ec_param_enc:named_curve

# Generate self-signed certificate (valid for 365 days)
openssl req -new -x509 -key server.key -out server.crt -days 365 -subj "/CN=localhost/O=Development/C=JP"
```

Specify the generated files in `config.toml`:

```toml
[tls]
cert_path = "./server.crt"
key_path = "./server.key"
```

> **Note**: In production, use certificates issued by a certificate authority such as Let's Encrypt.

## TLS Library

### rustls (Default)

- Memory-safe pure Rust implementation
- No additional dependencies
- Default when not using kTLS

### rustls + custom kTLS module (`--features ktls`)

- Performs TLS handshake with rustls
- After handshake completion, offloads to kTLS via the custom kernel TLS module (`src/ktls.rs`, `src/ktls_rustls.rs`)
- No additional external dependencies (pure Rust implementation)

```bash
# Build
cargo build --release --features ktls
```

## kTLS (Kernel TLS) Support

### Overview

kTLS is a Linux kernel feature that performs TLS data transfer phase encryption/decryption at the kernel level.
This project supports kTLS using rustls and a custom kernel TLS module (`src/ktls.rs`, `src/ktls_rustls.rs`).

### Performance Improvements

| Aspect | Effect |
|--------|--------|
| CPU Usage | 20-40% reduction (under high load) |
| Throughput | Up to 2x improvement |
| Latency | Reduced context switches |
| Zero-Copy | sendfile + TLS encryption |

### Enabling Procedure

```bash
# 1. Load kernel module
sudo modprobe tls

# 2. Build with ktls feature
cargo build --release --features ktls

# 3. Enable in config file (config.toml)
# [tls]
# ktls_enabled = true
# ktls_fallback_enabled = true  # optional
```

### Fallback Configuration

Control behavior when kTLS activation fails with `ktls_fallback_enabled`:

| Value | Behavior |
|-------|----------|
| `true` (default) | Continue with rustls on kTLS failure (graceful degradation) |
| `false` | kTLS required mode (reject connection on failure) |

**Benefits of disabling fallback (`ktls_fallback_enabled = false`):**

| Aspect | Effect |
|--------|--------|
| Performance Predictability | All connections guaranteed to use kTLS |
| Debug Ease | No mixed kTLS/rustls state |
| Early Environment Detection | Immediate failure when kTLS unavailable |

**Note:** When fallback is disabled, connections will fail in environments where kTLS is unavailable.
Verify the kernel module is loaded with `modprobe tls` beforehand.

```toml
[tls]
cert_path = "/path/to/cert.pem"
key_path = "/path/to/key.pem"
ktls_enabled = true
ktls_fallback_enabled = false  # kTLS required mode
```

### Requirements

- Linux 5.15 or higher (recommended, but works on earlier versions)
- `tls` kernel module loaded
- AES-GCM cipher suites (TLS 1.2/1.3)
- Built with ktls feature (`--features ktls`)

### Implementation Status

**With ktls feature enabled (`--features ktls`):**
- ✅ kTLS kernel module availability check
- ✅ Automatic kTLS activation after TLS handshake completion
- ✅ kTLS offload for both TX and RX
- ✅ Full async integration with monoio (io_uring)

**Default build (using rustls):**
- ❌ kTLS is not supported
- 👉 Build with `--features ktls` to use kTLS

### Security Considerations

| Risk | Mitigation |
|------|------------|
| Kernel Bugs | Pin kernel version, apply patches regularly |
| Session Key Exposure | TLS handshake runs in userspace (rustls) (maintains PFS) |
| DoS Attacks | Monitor kernel resources, rate limiting |

## TLS Certificate Hot Reload

Zero-downtime certificate rotation without restarting the proxy.

### How It Works

- A background thread polls certificate file `mtime` every `reload_interval_secs` seconds.
- When a change is detected, the new certificate is loaded into an `ArcSwap`.
- **Existing TLS connections** continue using the old certificate (no disruption).
- **New TLS handshakes** automatically pick up the new certificate.
- A `SIGHUP` signal also triggers an immediate reload of both config and certificates.
- **HTTP/1.1, HTTP/2, and HTTP/3 (QUIC/quiche) are all hot-reloadable** (F-105). Because each HTTP/3 worker owns its own `quiche::Config`, the reload thread publishes the raw cert/key PEM atomically via an `ArcSwap` — gated by a cheap per-iteration generation check so the event loop hot path is untouched. On **Linux** each worker swaps the PEM into its config through a `memfd` (Landlock-compatible, no filesystem access). On **FreeBSD/OpenBSD/macOS/Windows** (F-136) each worker rebuilds its `quiche::Config` entirely in memory via `Config::with_boring_ssl_ctx_builder`, feeding the PEM bytes straight into a `boring::ssl::SslContextBuilder` — no file, path, or memfd involved, which is what keeps reload working under FreeBSD capsicum capability mode (path-based `open`/`stat` — including the internal `fopen(3)` a path-based quiche API would perform — fails with `ECAPMODE` there). Existing QUIC connections keep the old certificate; only new handshakes present the new one. Once every worker has applied the update, the private-key plaintext is zeroed in memory (`secure_zero`).
- **Capability-mode / sandboxed reload for HTTP/1.1 and HTTP/2** (F-136): under FreeBSD capsicum capability mode, the mtime poll and PEM read for rustls's `ServerConfig` go through a single choke point (`tls_reload::pem_mtime`/`read_pem`) that switches to a dirfd opened before `cap_enter` (`security::capsicum::init_tls_cert_dirfds`) and reads via `openat`/`fstatat` with `O_RESOLVE_BENEATH`. Linux/macOS/Windows/OpenBSD are byte-for-byte unchanged (still plain `std::fs`).

### Configuration

```toml
[tls]
cert_path = "/etc/veil/cert.pem"
key_path  = "/etc/veil/key.pem"
# Zero-downtime certificate hot-reload
auto_reload = true
reload_interval_secs = 60  # Poll interval (default: 60s)
```

### Let's Encrypt Integration

```bash
# Renew certificate (certbot)
certbot renew --deploy-hook "touch /etc/veil/cert.pem"
# veil detects the mtime change and reloads automatically
```

> **Note**: If Landlock sandbox is enabled, the certificate directory must be included in `landlock_read_paths`.
