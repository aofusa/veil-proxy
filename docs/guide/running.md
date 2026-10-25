# Running & Operations

[← Documentation index](README.md) · [日本語](ja/running.md)

## Startup

```bash
# Start with the configuration file at the default path (see "Configuration file" below)
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
| `-c, --config <PATH>` | Path to config file | `$VEIL_CONFIG`, then the default path (below) |
| `-t, --test` | Test config file syntax and validity, then exit (nginx -t equivalent) | - |
| `-o, --override <KEY=VALUE>` | Override a config.toml value from the command line (repeatable, see below) | - |
| `-h, --help` | Show help message | - |
| `-V, --version` | Show version information | - |

### Configuration file

veil always reads a configuration file; there is no built-in configuration. The file is chosen in this order:

1. `-c FILE` / `--config FILE`
2. the `VEIL_CONFIG` environment variable (ignored when empty)
3. the default path:

   | Build | Default path |
   |---|---|
   | Linux, macOS, FreeBSD, OpenBSD, NetBSD | `/etc/veil/config.toml` |
   | Windows | `%ProgramData%\veil\config.toml` |
   | Built with `VEIL_DEFAULT_CONFIG_PATH=/abs/path` set at build time (BSD ports) | that path |

   `veil --help` prints the default path of the binary you run.

When the file does not exist, veil exits with status 1 and names the path it looked for. The startup log shows the
file and where its path came from (`Config File: /etc/veil/config.toml (from default path)`). A SIGHUP reload reads the
same file again.

#### Where the packages put the default configuration

Each package installs a default configuration and a default page, and veil reads them without any option. The default
configuration serves the page over plaintext HTTP/1.1 and h2c on `127.0.0.1:8080` (the container on `0.0.0.0:8080`) and
logs every request; the page also lists these paths (`/quickstart.html`).

| Installed with | Configuration | Page | How veil finds the file | Start |
|---|---|---|---|---|
| Container image | `/etc/veil/conf.d/config.toml` | `/var/www/` | the image's `CMD` passes `-c` | `docker run -p 8080:8080 ghcr.io/aofusa/veil` |
| deb / rpm | `/etc/veil/config.toml` | `/usr/share/veil/www/` | default path | Debian / Ubuntu: started on install; RPM: `sudo systemctl enable --now veil` |
| Homebrew | `$(brew --prefix)/etc/veil/config.toml` | `$(brew --prefix)/share/veil/www/` | the `veil` wrapper sets `VEIL_CONFIG` | `brew services start veil` or `veil` |
| winget | `%ProgramData%\veil\config.toml` | `%ProgramData%\veil\www\` | default path | `veil` |
| FreeBSD ports | `/usr/local/etc/veil/config.toml` | `/usr/local/share/veil/www/` | default path built in | `sysrc veil_enable=YES && service veil start` |
| OpenBSD ports | `/etc/veil/config.toml` | `/usr/local/share/veil/www/` | default path | `rcctl enable veil && rcctl start veil` |
| NetBSD pkgsrc | `/usr/pkg/etc/veil/config.toml` | `/usr/pkg/share/veil/www/` | default path built in | `veil=YES` in `/etc/rc.conf`, `service veil start` |

Package upgrades never overwrite a configuration you changed.

#### Installing

Each of these installs veil with the default configuration and page above, except the binary-only ones.

##### apt and dnf repositories

Signed repositories for Debian / Ubuntu (amd64, arm64) and RHEL-compatible / Fedora / Amazon Linux (x86_64,
aarch64). The signing key's fingerprint is `42FF D5A9 145A BEAD 9CDE  F997 AF5B 42F9 5AC9 AEB9`.

```bash
# Debian 12 / 13, Ubuntu 22.04 / 24.04
sudo install -d -m 0755 /etc/apt/keyrings
curl -fsSL https://aofusa.github.io/veil-packages/veil.gpg | sudo tee /etc/apt/keyrings/veil.gpg >/dev/null
echo "deb [signed-by=/etc/apt/keyrings/veil.gpg] https://aofusa.github.io/veil-packages/apt stable main" \
  | sudo tee /etc/apt/sources.list.d/veil.list
sudo apt update && sudo apt install veil      # starts the service
curl http://127.0.0.1:8080/

# RHEL 9 / 10 and compatibles, Fedora, Amazon Linux 2023
sudo curl -fsSL https://aofusa.github.io/veil-packages/veil.repo -o /etc/yum.repos.d/veil.repo
sudo dnf install veil
sudo systemctl enable --now veil
curl http://127.0.0.1:8080/
```

The `.deb` and `.rpm` files are also attached to each GitHub release. Logs go to the journal
(`journalctl -u veil`); reload a changed configuration with `sudo systemctl reload veil`. `apt remove` keeps
`/etc/veil/config.toml`, `apt purge` deletes it.

Upgrading from 0.8.x, which kept its configuration in `/var/etc/veil/config.toml`: the package leaves that file
in use (it writes `VEIL_CONFIG=/var/etc/veil/config.toml` to `/etc/default/veil` or `/etc/sysconfig/veil`).
To move to the default path, copy your settings into `/etc/veil/config.toml`, delete that line and run
`sudo systemctl restart veil`. 0.8.x also generated a self-signed certificate in `/var/etc/veil/ssl/`; it is
left as it was.

##### Homebrew (macOS, Linux)

```bash
brew install aofusa/tap/veil
brew services start veil        # or run `veil` in a terminal
curl http://127.0.0.1:8080/
```

The `veil` command is a wrapper that sets `VEIL_CONFIG` to `$(brew --prefix)/etc/veil/config.toml` unless you set
it yourself, so `veil -t` and `veil` use that file. `brew services restart veil` applies a changed configuration.

##### winget (Windows)

```powershell
winget install Aofusa.Veil      # as an administrator; installs for all users
veil                            # in a new terminal; then open http://127.0.0.1:8080/
```

The MSI installs `C:\Program Files\veil\veil.exe` (added to `PATH`), the configuration
`%ProgramData%\veil\config.toml` and the page `%ProgramData%\veil\www\`. Only administrators can change files
in `%ProgramData%\veil`. Upgrades and `winget uninstall` keep the configuration. veil is not registered as a
Windows service; to run it as one, use `sc.exe create veil binPath= "C:\Program Files\veil\veil.exe"` or a
service wrapper.

##### Rust (`cargo binstall`, `cargo install`)

```bash
cargo binstall veil-proxy                                   # the prebuilt binary of a GitHub release
cargo install veil-proxy --locked --features full           # build from crates.io (Linux)
```

Both install the `veil` binary only (see below). `cargo binstall` checks each archive's minisign signature.
For `cargo install`, pick the feature set for your OS: `full` (Linux), `full-macos`, `full-windows`,
`full-freebsd`, `full-openbsd`, `full-netbsd`; without `--features`, only the default features (kTLS, HTTP/2)
are built. Building needs a C compiler, `cmake` and, on Windows, `nasm` ([Building](building.md)).

#### Binary only: create the configuration yourself

The release archives, `cargo install veil-proxy` and `cargo binstall veil-proxy` install the `veil` binary only. They
do **not** install a configuration file or a page (cargo has no way to install data files, and veil does not write one
for you): create them yourself. If you want a default configuration and page installed for you, use a package or the
container image instead.

The release archive has an example in `examples/` to start from:

```bash
cd veil-<version>-<target>
./veil -c examples/config.toml        # serves examples/www/ on http://127.0.0.1:8080/
```

A minimal configuration, if you start from scratch (`/srv/www` holds your `index.html`):

```toml
[server]
listen_cleartext = "127.0.0.1:8080"   # plaintext HTTP/1.1 + h2c
enable_secure_redirect = false        # no TLS listener, so serve instead of redirecting to https

[access_log]
enabled = true

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/srv/www/"
```

Then run `veil -c /path/to/config.toml`, set `VEIL_CONFIG=/path/to/config.toml`, or put the file at the default path.
Relative paths in the file are relative to the directory you start veil from.

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

### Configuration Override via Environment Variables (`VEIL_OVERRIDE_<n>`)

The same overrides can be supplied through environment variables, which is convenient for
containers and systemd units (F-179). Put one override per variable, in exactly the
`--override` syntax, into `VEIL_OVERRIDE_<n>` where `<n>` is a positive decimal integer:

```bash
VEIL_OVERRIDE_1='server.listen = "0.0.0.0:8443"' \
VEIL_OVERRIDE_2='route.0.action.path = "/srv/www"' \
./veil -c /etc/veil/config.toml
```

| Rule | Behavior |
|------|----------|
| Name | `VEIL_OVERRIDE_<n>`; `<n>` must be a positive decimal integer (`VEIL_OVERRIDE_1`, `VEIL_OVERRIDE_20`). Any other suffix (`VEIL_OVERRIDE_`, `VEIL_OVERRIDE_X`, `VEIL_OVERRIDE_0`) is a startup error. |
| Order | Applied in ascending numeric order of `<n>` (`2` before `10`); when the same path appears twice, the later one wins. |
| Priority | `config.toml` < environment variables < `-o`/`--override` on the command line. |
| Reload | The environment is fixed at process start. On SIGHUP only `config.toml` is re-read and the same environment overrides are applied again. |
| Secrets | Do **not** pass private keys or tokens this way: the environment of a process is visible via `/proc/<pid>/environ` (Linux) or `ps -e` (BSD). Mount them as files instead (see [TLS in containers](tls.md#certificates-and-keys-in-containers)). |

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

### Settings that reload cannot change

A SIGHUP reload applies routes, upstreams, security and most other settings, but the listeners are fixed at startup.
Changing any of the following in the file and reloading does **not** take effect; veil logs one warning
(`Listener settings changed (...); restart veil to apply them`) naming the keys, and keeps the old listeners:

- `[server] listen`, `listen_cleartext`, `threads`
- `[server] http2_enabled` (the TLS ALPN list is built at startup; connections that negotiated h2 keep using HTTP/2)
- `[http3] enabled`, `listen`
- the `[[l4]]` listeners (`name`, `listen`, `protocol`)
- the `[[listener]]` entries (adding or removing one, `name`, `listen`, `tls`, `http2`, `http3`, `http3_listen`,
  `threads`, `unix_socket_permissions`). Their plaintext policy (`allow_http`, `allow_h2c`, `secure_redirect`,
  `redirect_to`) **is** applied by reload, like the `[server]` keys of `listen_cleartext`; when the listener set
  itself changed, the policies opened at startup stay in effect until the restart
- TLS certificates are reloaded separately (certificate hot reload), not by the configuration reload

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
2. Close idle keep-alive connections immediately (HTTP/1.1 connections waiting for the next request;
   HTTP/2 connections with no open stream receive `GOAWAY(NO_ERROR)`; HTTP/3 connections receive
   `GOAWAY` and are closed with `H3_NO_ERROR` once their in-flight requests finish)
3. Complete processing of in-flight requests (each connection closes after its last response;
   HTTP/2 and HTTP/3 refuse streams opened after the GOAWAY)
4. Wait for all worker threads to finish
5. Terminate process

HTTP/2 connections without open streams are also closed with `GOAWAY(NO_ERROR)` after 60 seconds of
inactivity during normal operation (the same idle timeout as HTTP/1.1 keep-alive).

The wait for existing connections is bounded by `[server] graceful_shutdown_timeout_secs` (default 30).
Connections still open after that are closed and the process exits normally (exit status 0).
On unix both signals are handled by a dedicated signal thread; in containers `docker stop` (SIGTERM)
therefore shuts veil down gracefully instead of waiting for SIGKILL.

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
> - `[[l4]]`, the plaintext listener (`listen_cleartext`), or HTTP/3 (the same rules decide at startup whether capability mode is entered at all);
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
