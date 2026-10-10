# Running & Operations

[← Documentation index](README.md) · [日本語](ja/running.md)

## Startup

```bash
# Start with default config file (/etc/veil/config.toml)
./veil

# Start with specified config file
./veil -c /path/to/config.toml
./veil --config /path/to/config.toml

# Show help
./veil --help

# Show version
./veil --version
```

### Command Line Options

| Option | Description | Default |
|--------|-------------|---------|
| `-c, --config <PATH>` | Path to config file | `/etc/veil/config.toml` |
| `-t, --test` | Test config file syntax and validity, then exit (nginx -t equivalent) | - |
| `-o, --override <KEY=VALUE>` | Override a config.toml value from the command line (repeatable, see below) | - |
| `-h, --help` | Show help message | - |
| `-V, --version` | Show version information | - |

### Configuration Override (`-o`/`--override`)

Any key in `config.toml` can be overridden from the command line at load time, without
editing the file. The option is repeatable (each `-o` supplies one override) and is
applied on startup, on `-t` validation, **and** on hot reload (SIGHUP) — the same
global override set is applied every time the config file is (re)parsed.

Syntax: `<path> = <toml-value>` (spaces around `=` are optional).

- `<path>` is a dotted key path, e.g. `server.threads`, `tls.cert_path`,
  `http3.mmsg_batch_size`. Each segment is one of:
  - a bare key: `[A-Za-z0-9_-]+`
  - a quoted string (`"..."` or `'...'`), needed when a key itself contains a dot
  - a decimal array index, used when the parent is an array, e.g. `l4.0.listen`
  - for convenience, any segment may be wrapped TOML-section-style in square
    brackets: `[server].threads = 1` is accepted and treated exactly like
    `server.threads = 1` (a single leading `[` / trailing `]` is stripped before
    the segment is interpreted).
- `<toml-value>` is parsed as a TOML value (so `1`, `"str"`, `true`, `1.5`,
  `[1, 2]`, `{ a = 1 }` all work as-is). If a bare, unquoted value fails to parse
  as TOML **and** contains none of `"`, `'`, `[`, `]`, `{`, `}`, it is retried once
  as a plain string literal — this lets you write
  `-o "tls.cert_path = /etc/veil/cert.pem"` without quoting the path. Values that
  contain those characters but still fail to parse as TOML are a hard error
  (no silent string fallback).

```bash
# Override a scalar
./veil -o "server.threads = 4"

# Bracket ([section].key) form, equivalent to the above
./veil -o "[server].threads = 4"

# String value without quotes
./veil -o "tls.cert_path = /etc/veil/cert.pem"

# Multiple overrides, including an array index
./veil -o "server.threads = 4" -o "l4.0.listen = 0.0.0.0:9000"
```

### Configuration Validation

Test your configuration file before deploying or reloading:

```bash
# Test default config file
./veil -t

# Test specific config file
./veil -t -c /path/to/config.toml
```

**Validation checks:**
- TOML syntax parsing
- Configuration value validation
- TLS certificate and key file existence

**Output examples:**
```bash
# Success
veil: configuration file config.toml test is successful

# Failure (TLS cert not found)
veil: configuration file config.toml test failed
veil: TLS certificate not found: /path/to/cert.pem
```

**Note**: When reloading configuration via SIGHUP, if the new configuration is invalid, the reload is rejected and the server continues running with the previous valid configuration.

## Configuration File Validation

Performs detailed validation of the configuration file at startup and outputs clear error messages if problems are found.

### Validation Items

| Item | Check Content |
|------|---------------|
| TLS Certificate | File existence check |
| TLS Private Key | File existence check |
| Listen Address | Valid socket address format |
| Upstream URL | Valid URL format |
| Proxy URL | Valid URL format |
| File Path | File/directory existence check |
| File Mode | `sendfile` or `memory` |

### Error Message Examples

```
Error: TLS certificate file not found: /path/to/cert.pem
Error: Invalid proxy URL for route 'example.com:/api/': invalid-url
Error: Upstream 'backend-pool' not found
```

## Graceful Shutdown

When receiving SIGINT (Ctrl+C) or SIGTERM, the server terminates safely:

1. Stop accepting new connections
2. Complete processing of existing requests
3. Wait for all worker threads to finish
4. Terminate process

```bash
# Start server
./veil -c ./examples/config.toml &

# Terminate safely
kill -SIGTERM $!
# or Ctrl+C
```

## Graceful Reload (Hot Reload)

When receiving SIGHUP, the server reloads the configuration file.
Existing connections are not interrupted, and new settings apply to new connections.

### Behavior

1. Receive SIGHUP signal
2. Reload config file specified at startup
3. Validate configuration
4. Lock-free configuration update via `ArcSwap`
5. New connections use new settings

> **Note**: On reload, the path specified with `-c` option at startup (or default `/etc/veil/config.toml`) is used.

> **FreeBSD capsicum capability mode — fail-closed** (F-181): when `capsicum_capability_mode = true` is set but capability mode cannot be entered — the configuration needs `bind(2)` after startup (`[[l4]]`, h2c, HTTP/3, the HTTP redirect listener), the connect broker cannot be started, `cap_enter(2)` fails, or the workers do not finish binding — veil logs the reason and **exits with status 1**. It does not silently fall back to the weaker rights-limited sandbox. Set `[security] allow_security_failures = true` to continue in rights-limited mode with a warning instead, as with the other sandboxes.

> **FreeBSD capsicum capability mode — proxying through the connect broker** (F-182): `Proxy` routes and `[upstreams]` work in capability mode. Before `cap_enter` (and after dropping privileges), veil starts a **connect broker**: a child process (veil re-executed, running as the same user) that stays outside the sandbox. The broker accepts only an index into an **allowlist fixed at startup** — every server in `[upstreams]`, every `Proxy` route URL, and the OpenTelemetry endpoint — starts a non-blocking `connect(2)`, and hands the socket back over `SCM_RIGHTS`; TLS and HTTP stay in the sandboxed process. A compromised worker therefore cannot use the broker to reach anything but the configured upstreams. Host names are resolved by the broker (re-resolved every 30 seconds). The extra cost applies only when a new upstream connection is opened (connections are pooled and multiplexed). The broker hardens itself (no ptrace or core dumps, cannot fork, cannot write files, dies with veil). If the broker exits, veil exits with status 1 so that the service manager restarts both.

> **FreeBSD capsicum capability mode** (`capsicum_capability_mode = true`): SIGHUP (and the admin reload) still re-reads the configuration file. Before `cap_enter`, veil opens the directory that holds the configuration file (and the access log directory) and later re-opens the file by name with `openat(2)` + `O_RESOLVE_BENEATH`, so replacing the file by rename and relative symlink swaps inside that directory (Kubernetes ConfigMap style) are picked up (F-178). TLS certificates reload the same way when `[tls] auto_reload = true` (F-136).
>
> A process in capability mode cannot open new directories, connect, or bind, so a reload that needs any of these is **rejected and the previous configuration is kept** (the log says `capability mode: ...; restart veil to apply this change`). Restart the process to apply:
>
> - a `File` route whose path is not under a directory that was a `File` route at startup (single-file routes outside such a directory included);
> - an upstream that is not in the connect broker's allowlist fixed at startup (a new `Proxy` route URL or `[upstreams]` server; changing routes, weights, or health checks for the same upstreams is fine);
> - `[[l4]]`, h2c, HTTP/3, or the HTTP redirect listener (the same rules decide at startup whether capability mode is entered at all);
> - enabling the access log or changing `[access_log] file_path`.
>
> Reopening the same access log path works, so `logrotate`-style "move, then SIGHUP" rotation is supported. The configuration directory stays readable to the process, so keep the configuration in a dedicated directory.

```bash
# Edit config file
vim examples/config.toml

# Reload configuration (zero downtime)
kill -SIGHUP $(pgrep veil)
```

### Supported Changes

| Item | Hot Reload Supported |
|------|---------------------|
| Routing configuration | ✅ |
| Security configuration | ✅ |
| Upstream configuration | ✅ |
| TLS certificates (HTTP/1.1, HTTP/2, HTTP/3) | ✅ (when `[tls] auto_reload = true`; see "TLS Certificate Hot Reload") |
| Listen address | ❌ (requires restart) |
| Worker thread count | ❌ (requires restart) |
