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

## TLS Session Resumption (F-183)

`[tls] session_resumption` selects how HTTP/1.1 and HTTP/2 clients resume TLS sessions
(HTTP/3 resumption is handled by quiche).

| Value | Behavior |
|---|---|
| `"tickets"` (default) | Stateless session tickets. The ticket key is shared by all workers, rotated automatically and kept across certificate reloads. TLS 1.3 resumption keeps no server-side state and takes no shared lock, so it works for any number of returning clients with constant memory. A `session_cache_size`-entry cache is also kept for TLS 1.2 clients that resume by session ID instead of tickets. |
| `"cache"` | Server-side session cache only (`session_cache_size` entries, shared by all workers). |
| `"off"` | No resumption (no tickets are sent). |

```toml
[tls]
session_resumption = "tickets"
session_cache_size = 4096   # entries ("cache", and TLS 1.2 session-ID clients with "tickets")
tls13_tickets = 1           # tickets sent per TLS 1.3 handshake (0 with "off")
```

Before F-183 veil used the rustls defaults: a 256-entry cache shared by all workers and two
TLS 1.3 tickets per handshake, each inserted into the cache. At a few thousand handshakes per
second that cache turns over in well under a second, so clients returning later could not
resume. Measured with `tools/perf/alloc_ab.sh` (TLS 1.3, OpenSSL 3 `wrk`, a new connection per
request): tickets 1,908 req/s vs the old default 1,839 req/s; without resumption 532 req/s.

Changing these keys requires a restart (certificate reloads keep the mode chosen at startup).

> Note on the bundled `williamyeh/wrk` image (LibreSSL 2.7, TLS 1.2 only, 2018): with
> `"tickets"` it resumes only every other connection (the client does not keep the ticket
> issued on a resumed handshake), so `h1tls_hs` drops from ~5,500 to ~1,300 req/s compared
> with `"cache"`. Current OpenSSL, BoringSSL, NSS, Go and rustls clients resume normally; if a
> fleet of such legacy clients matters more than unbounded TLS 1.3 resumption, use `"cache"`.

## Client Certificates and Upstream mTLS (F-217)

### Requiring client certificates

```toml
[tls]
cert_path = "/etc/veil/cert.pem"
key_path  = "/etc/veil/key.pem"
client_auth = "require"            # "none" (default) | "optional" | "require"
client_ca   = "/etc/veil/client-ca.pem"
client_allowed_sans = ["spiffe://cluster.local/ns/web/sa/frontend"]   # optional (F-220)
```

- `"require"` rejects clients that send no certificate, or one that does not chain to `client_ca`, during the
  handshake. `"optional"` asks for a certificate but also accepts clients without one (a certificate that is
  sent is still verified).
- Applies to TLS over TCP (HTTP/1.1 and HTTP/2). Combining it with `[http3]` is a startup error, because QUIC
  clients would bypass the check.
- `client_allowed_sans` accepts only certificates with one of the listed SANs (DNS name, URI or IP), checked once
  during the handshake; other identities from the same CA are rejected. It needs `client_auth = "optional"` or
  `"require"`.
- Certificate hot reload keeps the startup `client_auth` / `client_ca` / `client_allowed_sans`; changing them needs a
  restart.

### Client certificates towards upstreams

`[upstreams.<name>]` and the `Proxy` action accept:

| Key | Meaning |
|-----|---------|
| `tls_client_cert` / `tls_client_key` | client certificate and key (PEM) presented to the upstream; set both |
| `tls_ca` | trust anchors (PEM) for the upstream certificate; the public roots when omitted |
| `expected_san` | accepted SANs of the upstream certificate (DNS name, URI or IP; any match). When set, it replaces the host name check, e.g. for SPIFFE IDs |

```toml
[upstreams.backend]
servers = ["https://10.0.0.5:8443"]
tls_client_cert = "/etc/veil/upstream-client.pem"
tls_client_key  = "/etc/veil/upstream-client.key"
tls_ca          = "/etc/veil/upstream-ca.pem"
expected_san    = ["spiffe://cluster.local/ns/default/sa/backend"]
```

The TLS configuration is built when the configuration is loaded (not per connection). Pooled connections are
keyed by it, so upstreams with different client certificates never share a connection. A `SIGHUP` reload picks up
replaced certificate files. These keys take precedence over `tls_insecure`.

## TLS Certificate Hot Reload

Zero-downtime certificate rotation without restarting the proxy.

### How It Works

- A background thread polls the certificate and key files every `reload_interval_secs` seconds and reloads when their `mtime`, file identity (device + inode after following symlinks) or size **changes** — not only when they get newer. This also catches Kubernetes Secret updates, which swap a `..data` symlink to a new directory whose files may carry the same or an older `mtime` (F-180).
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

veil can issue and renew Let's Encrypt certificates by itself (ACME HTTP-01, `--features acme`, part of `full`).
See [ACME](acme.md). The rest of this section describes the alternative of using an external client such as certbot.

```bash
# Renew certificate (certbot)
certbot renew --deploy-hook "touch /etc/veil/cert.pem"
# veil detects the mtime change and reloads automatically
```

> **Note**: If Landlock sandbox is enabled, the certificate directory must be included in `landlock_read_paths`.

## Certificates and Keys in Containers

Mount certificates and private keys as **files** at run time instead of baking them into the image
or passing them through environment variables (F-180). The environment of a process is readable via
`/proc/<pid>/environ` and is hard to wipe from memory; files can be permission-restricted and rotated.

| Platform | How the files appear | Notes |
|----------|---------------------|-------|
| Docker secrets (`docker compose` / Swarm) | `/run/secrets/<name>` | See [`examples/container/compose.secrets.yaml`](../../examples/container/compose.secrets.yaml). The compose `file:` secret is a bind mount, so rotate it by overwriting the file in place. |
| Kubernetes Secret volume | `<mountPath>/tls.crt`, `tls.key` (symlinks into `..data/`) | The kubelet updates the volume by atomically swapping the `..data` symlink to a new timestamped directory. veil follows the swap (file identity check). See [`deploy/kubernetes/`](../../deploy/kubernetes/). Do not use `subPath` mounts — they never receive updates. |

```toml
[tls]
cert_path = "/run/secrets/tls_cert"      # or /etc/veil/tls/tls.crt on Kubernetes
key_path  = "/run/secrets/tls_key"
auto_reload = true
reload_interval_secs = 10

[security]
# Allow the whole secret directory, not the individual files: on Kubernetes the new contents live in
# a new directory under the mount point.
landlock_read_paths = ["/run/secrets", "/etc/veil", "/var/www"]
```

Only the *location* of a certificate may be set through `VEIL_OVERRIDE_<n>` (for example
`VEIL_OVERRIDE_1='tls.cert_path = "/etc/veil/tls/tls.crt"'`); never put PEM contents there.
`examples/container/e2e.sh` checks the whole flow (TLS with secrets, environment overrides,
certificate rotation, graceful stop) against a built image.
