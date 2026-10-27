# Security & Sandboxing

[← Documentation index](README.md) · [日本語](ja/security.md)

## Self-Sandboxing

This server has built-in **self-isolation from within the code** without using external tools like bubblewrap.

### Why In-Code Implementation Instead of External Tools?

| Approach | Pros | Cons |
|----------|------|------|
| bubblewrap (external) | Flexible configuration, existing tool | Additional dependency, configuration complexity |
| **This server (built-in)** | Zero dependencies, declared in code, automatic inheritance | Linux kernel dependent |

### Implemented Self-Isolation Features

#### 1. Landlock Filesystem Restriction (Linux 5.13+)

Process can declare "from now on, I will only access these directories."

```toml
[security]
enable_landlock = true
landlock_read_paths = ["/etc/veil", "/usr", "/lib", "/lib64"]
landlock_write_paths = ["/var/log/veil"]
```

**Supported ABI Versions:**

| ABI | Kernel | Features |
|-----|--------|----------|
| v1 | 5.13+ | Basic filesystem access control |
| v2 | 5.19+ | File reference permission (REFER) |
| v3 | 6.2+ | TRUNCATE permission |
| v4 | 6.7+ | ioctl permission |

Everywhere outside the listed paths, veil denies reading and writing as well as creating symlinks, sockets, FIFOs
and device nodes and executing files. Inside `landlock_write_paths` it allows regular files, directories and Unix
sockets, but still no symlinks, FIFOs, device nodes or execution. A listed path that cannot be opened, or whose
rule the kernel rejects, is logged as a warning at startup (access to it stays denied).

#### 2. seccomp System Call Restriction

Restricts system calls based on an allow list.

```toml
[security]
enable_seccomp = true
seccomp_mode = "filter"  # "log" / "filter" / "strict"
```

> The allowlist includes `faccessat2` (issued by glibc 2.33+/musl for `access()`/`faccessat()`) and
> `openat2` (used to resolve static files with `RESOLVE_BENEATH`, F-153). The container profiles
> (`docker/assets/security/seccomp.json` / `seccomp-epoll.json`) include both; `openat2` was added in
> v0.8.0 (without it veil falls back to `canonicalize()` on every request, and before B-154 the first
> static request after startup returned 404).
>
> For `watch = true` on the static file caches ([Compression & Cache](compression-cache.md#static-file-caches-follow-disk-changes)),
> the allowlist and both container profiles also include `inotify_init1`, `inotify_add_watch` and
> `inotify_rm_watch`; the in-process allowlist adds `getdents64` (the watcher thread walks the static root).
> Only the dedicated watcher thread issues them, at startup, on reload and when a directory is created.
> veil starts the watcher thread (like the workers) after the sandbox is applied.

**Which threads are sandboxed.** veil installs the seccomp filter with `SECCOMP_FILTER_FLAG_TSYNC`, so it applies to
every thread of the process, including the control-plane threads started before it (logging, configuration and
certificate reload, VDS, ACME, health checks, cache cleanup, Proxy-Wasm tick and gRPC). Landlock has no such flag and
applies to the thread that installs it and to threads created afterwards: the main thread, the workers and the static
file watcher. Of the threads started earlier, the health check, Proxy-Wasm tick and Proxy-Wasm gRPC threads restrict
themselves with the same ruleset; the configuration reload, certificate reload, VDS and ACME threads stay outside
Landlock (they read the configuration, certificates and Proxy-Wasm modules, and ACME writes certificates) and are
covered by seccomp only. Before v0.10.0 the seccomp filter, too, covered only the main thread and the threads created
after it.

**Recommended Deployment Procedure:**

```bash
# 1. First verify with log mode
enable_seccomp = true
seccomp_mode = "log"

# 2. Check blocked system calls
journalctl -f | grep -i seccomp

# 3. Switch to filter mode if no issues
seccomp_mode = "filter"
```

#### 3. Privilege Dropping

After starting as root and creating listeners, drop to unprivileged user.

```toml
[security]
drop_privileges_user = "veil"
drop_privileges_group = "veil"
```

### About Namespace Isolation

> **Note**: Namespace isolation like `unshare(CLONE_NEWNET)` is **not recommended** for reverse proxies.
> Isolating the network namespace will break proxy functionality.
> 
> If namespace isolation is required, we recommend doing it at the **systemd level** (see below).

## JWT Authentication (F-220)

`[route.jwt]` verifies a JSON Web Token on every request of the route before it reaches the action (requires the
`jwt` feature, included in `full*`). Requests without a token get `401` (`Jwt is missing`), tokens that do not verify
get `401` (`Jwt verification fails`), and verified tokens that do not satisfy `require_claims` get `403`
(`RBAC: access denied`).

```toml
[[route]]
[route.conditions]
path = "/api/*"
[route.action]
type = "Proxy"
upstream = "api"
[route.jwt]
issuer = "https://auth.example.com"       # checked against "iss" (empty: not checked)
audiences = ["api"]                       # one of them must be in "aud" (empty: not checked)
jwks_path = "/etc/veil/jwks.json"         # or jwks = '{"keys":[...]}' inline
# header = "authorization"                # default
# prefix = "Bearer "                      # default (case-insensitive)
# allow_missing = false                   # true: requests without a token pass (a token present is still verified)
# require_expiration = false
# clock_skew_secs = 60
require_claims = { scope = ["read"] }     # every claim must have one of the values
```

- Signatures: RS256/384/512, PS256/384/512 and ES256/384 with RSA and EC (P-256, P-384) keys from the JWKS. `exp`
  and `nbf` are checked with the clock skew.
- A claim that is a string is split on spaces (OAuth `scope`), arrays match any element, booleans and numbers match
  their text form.
- The JWKS is read when the configuration is loaded; replace the file and send `SIGHUP` to rotate keys. veil does not
  fetch remote JWKS URIs.
- CORS preflights answered by `[route.traffic.cors]` do not need a token.
- For identity-based authorization of TLS clients, use `[tls] client_allowed_sans` ([TLS](tls.md)).

## Security Hardening (systemd Sandboxing)

io_uring is a powerful async I/O interface, but if exploited, it poses a risk of kernel privilege escalation.
This server can achieve robust security when combined with systemd's sandboxing features.

### Security Architecture (Defense in Depth)

```
┌─────────────────────────────────────────────────────────────────┐
│ systemd (PID 1) - Outer isolation layer                         │
│ ┌─────────────────────────────────────────────────────────────┐ │
│ │ Namespace isolation (ProtectSystem, PrivateTmp, PrivateDevices) │ │
│ │ ┌─────────────────────────────────────────────────────────┐ │ │
│ │ │ veil built-in security                                  │ │ │
│ │ │ ┌─────────────────────────────────────────────────────┐ │ │ │
│ │ │ │ Landlock (filesystem restriction)                   │ │ │ │
│ │ │ │ ┌─────────────────────────────────────────────────┐ │ │ │ │
│ │ │ │ │ seccomp (system call restriction)               │ │ │ │ │
│ │ │ │ │ ┌─────────────────────────────────────────────┐ │ │ │ │ │
│ │ │ │ │ │ Application (io_uring + rustls)             │ │ │ │ │ │
│ │ │ │ │ │ - Allow: io_uring_*, socket, read, write... │ │ │ │ │ │
│ │ │ │ │ │ - Deny: fork, execve, ptrace, mount...      │ │ │ │ │ │
│ │ │ │ │ └─────────────────────────────────────────────┘ │ │ │ │ │
│ │ │ │ └─────────────────────────────────────────────────┘ │ │ │ │
│ │ │ └─────────────────────────────────────────────────────┘ │ │ │
│ │ └─────────────────────────────────────────────────────────┘ │ │
│ └─────────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
```

### Required System Calls

Minimum system calls required for this server to operate:

| Category | System Calls | Purpose |
|----------|--------------|---------|
| **io_uring** | `io_uring_setup`, `io_uring_enter`, `io_uring_register` | monoio runtime |
| **Network** | `socket`, `bind`, `listen`, `accept4`, `connect`, `sendto`, `recvfrom`, `sendmsg`, `recvmsg`, `setsockopt`, `getsockopt` | TCP/UDP sockets |
| **File I/O** | `openat`, `read`, `write`, `close`, `fstat`, `readv`, `writev` | Config, certificates, logs |
| **Files and directories** | `dup`, `dup3`, `unlink(at)`, `mkdir(at)`, `rename(at)`, `renameat2`, `chmod`, `fchmod(at)` | Unix socket listeners, disk spill-over of request bodies, ACME certificate storage |
| **Memory** | `mmap`, `munmap`, `mprotect`, `brk`, `madvise`, `mremap`, `mlock`, `mlock2` | mimalloc, Huge Pages, io_uring registered buffers |
| **Threads** | `clone`, `clone3`, `futex`, `exit_group`, `set_tid_address` | Worker threads |
| **CPU Affinity** | `sched_setaffinity`, `sched_getaffinity` | CPU pinning |
| **Signals** | `rt_sigaction`, `rt_sigprocmask`, `rt_sigreturn` | SIGTERM/SIGHUP |
| **Time** | `clock_gettime`, `nanosleep` | Timeouts |
| **Other** | `prctl`, `ioctl`, `getrandom`, `fcntl`, `uname`, `landlock_restrict_self` | Various control; control-plane threads restricting themselves |

### systemd Service File

A sandbox-enabled service file is provided at `contrib/systemd/veil.service`.

#### Installation

For automated setup, use the `.deb` package ([packaging/README.md](../../packaging/README.md)).
The steps below are for manual installation.

```bash
# 1. Create dedicated user
sudo useradd -r -s /sbin/nologin veil

# 2. Create directories
sudo mkdir -p /var/etc/veil/ssl
sudo mkdir -p /var/log/veil /var/cache/veil /var/tmp/veil
sudo chown -R veil:veil /var/log/veil /var/cache/veil /var/tmp/veil

# 3. Copy configuration files
sudo cp contrib/config/config.toml /var/etc/veil/config.toml
sudo cp server.crt /var/etc/veil/ssl/cert.pem
sudo cp server.key /var/etc/veil/ssl/key.pem
sudo chown veil:veil /var/etc/veil/ssl/key.pem
sudo chmod 600 /var/etc/veil/ssl/key.pem
sudo chown -R root:veil /var/etc/veil
sudo chmod 0640 /var/etc/veil/config.toml

# 4. Install binary
sudo cp target/release/veil /usr/bin/veil

# 5. Install service file
sudo cp contrib/systemd/veil.service /etc/systemd/system/
sudo systemctl daemon-reload

# 6. Enable and start service
sudo systemctl enable veil
sudo systemctl start veil
```

#### Important Configuration Items

```ini
[Service]
# === User ===
User=veil
Group=veil

# === Resource Limits ===
# io_uring registered buffers require memory lock
LimitMEMLOCK=infinity
LimitNOFILE=1048576

# === Filesystem Isolation ===
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
LogsDirectory=veil
CacheDirectory=veil
ReadOnlyPaths=/var/etc/veil

# === Namespace Isolation ===
RestrictNamespaces=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectKernelTunables=yes

# === Network ===
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK

# === Security Hardening ===
NoNewPrivileges=yes
# MemoryDenyWriteExecute=yes  # disabled for WASM (full build)
RestrictSUIDSGID=yes

# === System Call Restriction ===
# Delegated to config.toml seccomp/Landlock (SystemCallFilter omitted)
SystemCallErrorNumber=EPERM
```

### Enabling Huge Pages

To maximize io_uring and mimalloc performance, enable Huge Pages.

```bash
# 1. Reserve Huge Pages (128 * 2MB = 256MB)
echo 128 | sudo tee /proc/sys/vm/nr_hugepages

# 2. Persist
echo "vm.nr_hugepages=128" | sudo tee -a /etc/sysctl.d/99-veil.conf
sudo sysctl -p /etc/sysctl.d/99-veil.conf

# 3. Remove MEMLOCK limit in systemd
# Set LimitMEMLOCK=infinity in veil.service
```

### Security Verification

How to verify the service's security state:

```bash
# Verify configuration with systemd-analyze
systemd-analyze security veil.service

# Check running security state
cat /proc/$(pgrep veil)/status | grep -E "Seccomp|NoNewPrivs|CapBnd"

# Expected output:
# Seccomp:        2                    # seccomp filter enabled
# NoNewPrivs:     1                    # Cannot gain new privileges
# CapBnd:         0000000000000c00     # Only CAP_NET_BIND_SERVICE
```

### Troubleshooting

#### io_uring Not Working

```bash
# Cause: System calls being blocked
# Solution: Add io_uring_* to SystemCallFilter
journalctl -u veil | grep -i "seccomp"

# Manual test
sudo strace -f -e trace=io_uring_setup /usr/local/bin/veil -c /etc/veil/config.toml
```

#### Memory Lock Failure

```bash
# Cause: MEMLOCK limit too low
# Solution: Set LimitMEMLOCK=infinity
cat /proc/$(pgrep veil)/limits | grep "locked memory"
```

#### Cannot Bind to Privileged Ports (443/80)

```bash
# Cause: Missing CAP_NET_BIND_SERVICE
# Solution 1: Configure in systemd
#   AmbientCapabilities=CAP_NET_BIND_SERVICE

# Solution 2: Grant capability to binary
sudo setcap 'cap_net_bind_service=+ep' /usr/local/bin/veil
```

### Alternative: Using with bubblewrap

For stricter isolation, combine systemd with bubblewrap:

```ini
[Service]
ExecStart=/usr/bin/bwrap \
    --ro-bind /usr /usr \
    --ro-bind /lib /lib \
    --ro-bind /lib64 /lib64 \
    --ro-bind /etc/veil /etc/veil \
    --bind /var/log/veil /var/log/veil \
    --unshare-pid \
    --die-with-parent \
    /usr/local/bin/veil -c /etc/veil/config.toml
```

In this configuration, systemd creates the outer "container" and bubblewrap provides an even stricter filesystem view.

## Panic Recovery

Veil implements connection-level panic catching to ensure high availability.

### Behavior

When a panic occurs during request processing:

| Scenario | Impact |
|----------|--------|
| **Without panic recovery** | Worker thread crashes, all connections on that worker are terminated |
| **With Veil's panic recovery** | Only the affected connection terminates, other connections continue normally |

### Implementation

- Uses `std::panic::catch_unwind` to wrap each connection's async task
- Panics are caught at the poll level and logged as errors
- `ConnectionGuard` ensures the connection counter is correctly decremented even on panic
- Worker threads remain alive and continue accepting new connections

### Logged Output

When a panic is caught:
```
[ERROR] Task panicked during poll: Any { .. }
```

### Notes

- This feature is automatically enabled; no configuration required
- Only protects against panics inside `monoio::spawn` tasks
- Panics in the accept loop or runtime initialization still terminate the worker thread

## Security Audit (F-236)

Before each release the repository is scanned with Anthropic's
[claude-security](https://github.com/anthropics/claude-plugins-official/tree/main/plugins/claude-security) plugin.
Confirmed findings are fixed together with a `B-` ticket in `docs/backlog/bugs/`; the v0.8.0 audit and its re-scan produced B-135 to B-151.

How to re-run the scan (from a clean worktree of the revision to audit, so the scan cannot see uncommitted files):

```bash
git worktree add ../veil-secscan <revision>
cd ../veil-secscan
CLAUDE_CODE_WORKFLOWS=1 claude -p \
  "/claude-security:claude-security Scan codebase at medium effort. I understand it will use a lot of tokens." \
  --permission-mode auto
```

The report (`CLAUDE-SECURITY-RESULTS.md`, `.jsonl`, `.sarif`) is written to the worktree. Keep it under
`docs/artifacts/claude-security/` (not committed) until the findings are fixed. Compare a re-scan with the previous
one by `claudeSecurityPluginFindingId`; findings judged to be false positives are recorded with the reason in
`docs/artifacts/claude-security/triage.md`.

Request-level hardening that came out of the audit:

- HTTP/2 and HTTP/3 field names must be tokens and values must not contain NUL, CR or LF or start/end with
  whitespace; `:method` must be a token and `:path` must not contain spaces or control characters (RFC 9113 §8.2.1,
  RFC 9114 §4.2). Violations are stream errors, so they can never reach an HTTP/1.1 upstream.
- `remove_request_headers` / `add_request_headers` apply to every client and upstream protocol.
- The response cache does not store `Set-Cookie` responses, responses that `Vary` on headers outside `key_headers`,
  or responses to `Authorization` requests (unless `key_headers` includes `Authorization`).
- IP allow/deny lists match IPv4-mapped IPv6 addresses as IPv4.
- Request paths are normalized before routing and `[route.security]` checks and forwarded normalized
  (`[security] path_normalization`); absolute-form targets become origin-form, other non-origin targets get 400.
- Chunked request bodies are parsed strictly (RFC 9112 §7.1; malformed framing is rejected) and nothing after
  the terminating chunk is forwarded upstream as part of that request.
- WebSocket upgrades apply `remove_request_headers` / `add_request_headers`; client bytes that follow the
  handshake are sent upstream only after a `101`.
- HTTP/3 caps connections per worker (`[http3] max_connections`), answers Initials with a stateless Retry once
  `retry_threshold` connections are handshaking, and drops connections that do not finish the handshake within
  `handshake_timeout`, so a flood of Initials from spoofed addresses cannot allocate connections without bound.
- HTTP/2 and HTTP/3 buffered request bodies are capped while they accumulate (the largest
  `max_request_body_size` of any route, at least 10 MB); the stream is reset beyond it.
- The Linkerd admin endpoints `/shutdown` and `/env.json` answer only loopback clients.
- The mesh normalizes paths before routing/RBAC/JWT and derives `remoteIpBlocks` from Envoy's original client address.
