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

#### 2. seccomp System Call Restriction

Restricts system calls based on an allow list.

```toml
[security]
enable_seccomp = true
seccomp_mode = "filter"  # "log" / "filter" / "strict"
```

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
| **Memory** | `mmap`, `munmap`, `mprotect`, `brk`, `madvise`, `mremap`, `mlock`, `mlock2` | mimalloc, Huge Pages, io_uring registered buffers |
| **Threads** | `clone`, `clone3`, `futex`, `exit_group`, `set_tid_address` | Worker threads |
| **CPU Affinity** | `sched_setaffinity`, `sched_getaffinity` | CPU pinning |
| **Signals** | `rt_sigaction`, `rt_sigprocmask`, `rt_sigreturn` | SIGTERM/SIGHUP |
| **Time** | `clock_gettime`, `nanosleep` | Timeouts |
| **Other** | `prctl`, `ioctl`, `getrandom`, `fcntl`, `uname` | Various control |

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
