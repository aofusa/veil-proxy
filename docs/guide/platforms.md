# Platform Support & Runtime Backends

[← Documentation index](README.md) · [日本語](ja/platforms.md)

## Platform Support & Runtime Backends (F-120 / F-125)

The data-plane runtime backend is selected at **compile time** (no runtime dispatch, no
hot-path cost). The default is unchanged (Linux io_uring).

| Platform | Runtime backend | Native security | kTLS | Notes |
|----------|-----------------|-----------------|------|-------|
| **Linux (default)** | io_uring (`src/runtime/uring/`) | seccomp + Landlock + CBPF | ✅ (Linux 5.15+) | Default features unchanged; performance non-regressed |
| **Linux `--features epoll`** | epoll readiness reactor (`src/runtime/reactor/`) | seccomp (epoll syscalls; io_uring syscalls dropped) + Landlock | ✅ | Fallback for hosts without io_uring |
| **FreeBSD (x86_64/aarch64)** | kqueue readiness reactor (optionally POSIX AIO with `--features aio`, F-127) | capsicum (`cap_rights_limit` / `cap_enter`) + jail | ✅ (FreeBSD 13.0+, `TCP_TXTLS_ENABLE`/`TCP_RXTLS_ENABLE`; F-126). **But do not enable it for large responses**: FreeBSD's software kTLS dispatches each TLS record's encryption to kernel worker threads, which measured **26% slower** on 54KB responses and pins throughput at 1.2 GB/s through serialization (default is false; see [docs/perf/README.md](../perf/README.md)) | `[security] enable_capsicum`, `capsicum_capability_mode`, `jail_name`. TLS cert hot-reload (H1/H2 and HTTP/3) keeps working under capsicum capability mode (F-136): cert/key parent directories get a dirfd opened before `cap_enter`, and reads go through `openat`/`fstatat` (`O_RESOLVE_BENEATH`). `http3` (quiche) now uses `boringssl-boring-crate` (external `boring` crate) instead of sharing `aws-lc-sys` with rustls, so its in-memory `SSL_CTX` API can rebuild certificates without ever opening a path |
| **OpenBSD (x86_64/aarch64)** | kqueue readiness reactor | pledge + unveil | ✗ (userspace rustls) | `[security] enable_pledge`, `enable_unveil`. TLS uses the **ring** rustls provider (aws-lc-rs can't complete handshakes on OpenBSD; F-122). **WASM runs via the Pulley interpreter** with the on-demand instance allocator and `MAP_STACK` fiber stacks (B-52; wasmtime's pooling allocator silently ignores `with_host_stack`, and OpenBSD kills any process whose SP is outside a `MAP_STACK` mapping). Pulley emits no native code, so no `wxallowed` filesystem is required — at interpreter speed. On aarch64, wasmtime itself is also switched to a vendored build (see NetBSD row below and B-55) since it's part of the same signal-handling gap; behavior is otherwise identical to x86_64. HTTPS static/proxy serving verified 200 |
| **NetBSD (x86_64/aarch64)** | kqueue readiness reactor | **chroot(2) + privilege drop only — no pledge/unveil equivalent** (F-140) | ✗ (userspace rustls) | `[security] chroot_dir` (opt-in `chroot(2)` + `chdir("/")`, applied before `drop_privileges_user`/`drop_privileges_group`). NetBSD has no runtime API equivalent to OpenBSD's pledge/unveil (Veriexec is a kernel-config/load-time integrity mechanism, not a per-process syscall filter; `secmodel_securelevel` is a system-wide boot-time setting) — veil logs this limitation honestly at startup (`security::netbsd::report_security_support`) rather than pretending to sandbox syscalls. TLS uses the same **ring** rustls provider + **`boringssl-boring-crate`** quiche backend as OpenBSD. **Proxy-Wasm is available on NetBSD (all architectures), FreeBSD aarch64, and OpenBSD aarch64** (B-55 resolved): crates.io wasmtime 40's signal-based trap handling has no `ucontext` branch for these three targets — even x86_64 NetBSD fails wasmtime's own build with `error: unsupported platform` in `signals.rs`, and the Pulley interpreter alone can't route around it (the failure happens in wasmtime's build script, before `Config::target` is ever consulted). The fix is a vendored copy of crates.io wasmtime 40.0.4 at `third_party/wasmtime` (package renamed `veil-wasmtime`, `[lib] name` kept as `wasmtime` so no `src/` import changes; see `third_party/wasmtime/README.veil.md`) with a 2-line `build.rs` delta that forces `has_native_signals = false` on exactly these three targets, selected via Cargo target-specific dependencies. All other platforms (Linux/Windows/macOS/FreeBSD x86_64/OpenBSD x86_64) keep using unmodified crates.io wasmtime 40.0.0. These three targets always run WASM through the **Pulley interpreter** (no native JIT, so no signal-based traps are needed in the first place). `full-freebsd`/`full-openbsd`/`full-netbsd` all include the `wasm` feature (the feature sets are architecture-independent — the vendored-wasmtime switch is made by Cargo target-specific dependencies, not by features). **NetBSD additionally requires `paxctl +m <binary>` before WASM filters will execute** (B-60): NetBSD enforces PaX MPROTECT system-wide (`security.pax.mprotect.enabled`/`.global` = 1), which makes wasmtime's runtime `mmap`/`mprotect` of executable code memory fail with `EACCES` even under the Pulley interpreter — `paxctl(8)` must explicitly disable MPROTECT restrictions on the veil binary. This is wired into `tests/e2e_setup.sh` (automatic when running on NetBSD), `tools/qemu/bsd-vm.sh` (`cmd_build` applies it to the freshly built guest binary), and `packaging/scripts/build-bsd.sh` (applied when packaging from a NetBSD host, otherwise documented as a required post-install step). Verified on real NetBSD 10.1 (x86_64 and evbarm-aarch64) |
| **macOS (x86_64/aarch64, universal2)** | kqueue readiness reactor (reused from FreeBSD/OpenBSD/NetBSD) | `sandbox_init` (Seatbelt) | ✗ (userspace rustls) | `[security] enable_sandbox_macos`. TLS uses the **aws_lc_rs** rustls provider; `http3` (quiche) uses its own bundled BoringSSL (F-131). Cross-built with `docker/Dockerfile.macos` (`cargo zigbuild --target universal2-apple-darwin`, `--features full`); verified on real hardware by the maintainer |
| **Windows (x86_64-pc-windows-msvc / aarch64-pc-windows-msvc)** | WSAPoll readiness reactor (`src/runtime/reactor/wsapoll.rs`, `src/runtime/reactor/tcp/windows.rs`, Winsock) | Job Object (best-effort) | ✗ (userspace rustls) | `[security] enable_job_object_windows`. TLS uses the **aws_lc_rs** rustls provider on both archs; `http3` (quiche) uses its own bundled BoringSSL (F-131). Cross-built with `docker/Dockerfile.windows` (`cargo xwin build --target <target>`, `--features full`; `packaging/scripts/build-cross.sh --target windows` builds both archs); verified on real hardware by the maintainer |

- The backend is chosen by `build.rs`-emitted cfgs (`veil_rt_uring` / `veil_rt_reactor` and
  `veil_poller_epoll` / `veil_poller_kqueue`). The public runtime API paths
  (`runtime::tcp`, `runtime::executor`, `runtime::timer`, …) are identical across backends.
- `--features epoll` is Linux-only (build.rs errors on other targets, where kqueue is
  selected automatically). Non-target security keys are accepted and ignored with a warning.
- **aarch64-linux**: cross-built via `docker/Dockerfile.{glibc,musl}.aarch64`
  (QEMU user-mode verified; QEMU lacks io_uring, so QEMU runs use the `epoll` build).
- **FreeBSD/OpenBSD** are built inside a matching QEMU VM — `tools/qemu/bsd-vm.sh <os> <arch>`
  covers FreeBSD/OpenBSD × x86_64/aarch64 (setup → build → `tests/e2e_setup.sh test` →
  fetch the binary), and `packaging/scripts/build-bsd.sh` turns the binary into a tar.gz with
  rc.d/jail.conf. **NetBSD support (F-140) is code-complete but not yet wired into
  `bsd-vm.sh`/`build-bsd.sh`** — QEMU VM setup and E2E verification for NetBSD are tracked
  as follow-up work in `docs/backlog/features/F-140-netbsd-support.md`; this environment
  also lacks a NetBSD cross C toolchain, so `cargo check --target x86_64-unknown-netbsd`
  currently fails inside `ring`'s/`boring`'s native build scripts before reaching veil's own
  code. **FreeBSD has no Docker cross-build path** — `docker/Dockerfile.freebsd` and
  `build-cross.sh --target freebsd` were removed because the build **fails at link
  time**: `aws-lc-sys` assembles none of its s2n-bignum `.S` files under a FreeBSD cross
  configuration, producing many `undefined symbol: curve25519_x25519_byte`-style errors
  (see `docs/backlog/bugs/B-49-...`, unresolved — the ticket documents the removal).
  **QEMU VM native build is the sole official FreeBSD build path**, for both
  architectures (`aarch64-unknown-freebsd` is additionally Rust Tier 3 with no prebuilt
  std, so it could never have used Docker regardless):
  `tools/qemu/bsd-vm.sh freebsd <arch> build` → `fetch` →
  `packaging/scripts/build-bsd.sh --os freebsd --arch <arch> --binary <path>`.
- **FreeBSD POSIX AIO (`--features aio`, F-127)**: opt-in build-time switch, **not recommended**
  (FreeBSD only;
  build.rs panics on other targets, same pattern as `epoll`). Replaces the default kqueue
  readiness `TcpStream::read`/`write` with `aio_read(2)`/`aio_write(2)` completion-based I/O,
  with completions delivered through the same kqueue loop via `EVFILT_AIO`
  (`aio_sigevent.sigev_notify = SIGEV_KEVENT`). Falls back to the readiness path per-call on
  `EAGAIN` (AIO daemon pool/queue limits). Not part of `--features full`, and **no longer part
  of `full-freebsd` either (B-63)**: measured on FreeBSD 14.3 aarch64
  it is strictly worse than the readiness path — POSIX AIO needs 3 syscalls per I/O
  (submit + `aio_error` + `aio_return`) versus 1, costing 78% of small-response HTTP/1.1 TLS
  throughput (105,520 vs 187,374 rps) and ~80% of small-response L4 TCP throughput
  (37k vs 202k rps), with no measurable gain on large (54KB) responses — and it makes the
  server stall completely under concurrent small HTTP/2 responses. See
  `docs/backlog/bugs/B-63-freebsd-aio-h2-stall.md`,
  `docs/backlog/features/F-127-freebsd-aio.md` and
  `docs/artifacts/f127_freebsd_aio_design.md` for the design and verification notes.
- **macOS (F-125/F-131)**: cross-built via Docker (`docker/Dockerfile.macos`,
  `messense/cargo-zigbuild`) — see
  `packaging/scripts/build-cross.sh --target macos`. macOS lacks
  `accept4`/`MSG_NOSIGNAL`/`pipe2`/`SOCK_NONBLOCK|SOCK_CLOEXEC`; `reactor/tcp.rs` and
  `runtime/udp.rs` fall back to plain `socket`/`accept` + `fcntl` and `SO_NOSIGPIPE`, and
  `runtime/offload.rs` falls back to `pipe` + `fcntl`. `--features full` (including
  `http3` and `wasm`) is the default for `build-cross.sh --target macos` (F-131).
- **Windows (F-125/F-131, v0.6.0)**: cross-built via Docker (`docker/Dockerfile.windows`,
  `messense/cargo-xwin`) — see
  `packaging/scripts/build-cross.sh --target windows`, which builds both
  x86_64-pc-windows-msvc and aarch64-pc-windows-msvc with `--features full`
  (`http3` via bundled BoringSSL, plus `wasm` and `l4-proxy`). `ktls` is Linux/FreeBSD only.
- **TLS crypto provider** is selected per target in `src/tls_provider.rs` (F-122/F-131/F-140):
  **OpenBSD/NetBSD use rustls's `ring`** provider (aws-lc-rs cannot complete TLS handshakes
  on OpenBSD; NetBSD is assumed to share the same Tier-3 risk and was conservatively matched
  to OpenBSD rather than independently verified), **every other target
  (Linux/FreeBSD/macOS/Windows) uses `aws_lc_rs`**.
  `Cargo.toml` splits the provider via target-specific dependencies plus `resolver = "2"`;
  keep the two in sync.
- **HTTP/3 (quiche) crypto backend** is likewise split per target:
  **Linux** builds quiche with `default-features = false` so it **shares the same
  `aws-lc-sys`** as rustls (unchanged, memfd-based cert loading), while
  **FreeBSD/macOS/Windows/OpenBSD/NetBSD** build quiche with the **`boringssl-boring-crate`**
  feature (external `boring` crate, bundled BoringSSL). This is what `AWS_LC_SYS_NO_PREFIX`
  selects — see the note in the packaging section and
  [`.cargo/config.toml`](../../.cargo/config.toml) (B-47).
- **F-136 (TLS cert hot reload under sandboxing)**: on FreeBSD/OpenBSD/macOS/Windows,
  HTTP/3 certificate (re)loading uses quiche's in-memory `SSL_CTX` API
  (`Config::with_boring_ssl_ctx_builder`) — the PEM bytes already held in memory are fed
  straight into a `boring::ssl::SslContextBuilder`, with no file, path, or memfd involved.
  This is what makes certificate hot reload work even under FreeBSD capsicum capability
  mode, where any path-based `open`/`stat` (including the `fopen(3)` that
  `load_cert_chain_from_pem_file` performs internally) fails with `ECAPMODE`. Linux keeps
  the original memfd + `/proc/self/fd/N` path unchanged; mixing `aws-lc-sys`
  (`NO_PREFIX=1`) with the external `boring` crate in one binary was tested and fails at
  link time with duplicate BoringSSL/AWS-LC symbols, which is why FreeBSD was moved off
  the shared-`aws-lc-sys` scheme instead of being added to it. See
  `docs/artifacts/f136_platform_design.md` for the experiment and rejected alternatives.
