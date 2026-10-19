# Building & Packaging

[← Documentation index](README.md) · [日本語](ja/building.md)

## Build

### Dependencies

The following system libraries are required depending on enabled features:

| Dependency | Required for | Notes |
|------------|-------------|-------|
| `cmake` | `http3` feature (aws-lc-sys libssl, shared with quiche) | Must be installed before building |
| `nasm` | `aws-lc-rs` (TLS, always required) | Assembly optimizations for crypto |

On Debian/Ubuntu:

```bash
apt-get install -y cmake nasm
```

### Local Build

```bash
# Default build — kTLS + HTTP/2 + mimalloc (recommended)
cargo build --release

# Full featured build — all optional features enabled
cargo build --release --features full

# Minimal build — no optional features
cargo build --release --no-default-features
```

The binary is generated at `target/release/veil`.

### Distribution Build (glibc 2.28, Docker)

To produce a binary compatible with older Linux distributions (glibc ≥ 2.28), use
[`messense/cargo-zigbuild`](https://github.com/messense/cargo-zigbuild) which links against a minimum glibc version via the Zig toolchain.

```bash
# Full featured build
docker run --rm -it -v $(pwd):/io -w /io messense/cargo-zigbuild bash -c \
  "apt-get update -y && apt-get install -y cmake nasm && \
   cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.28 --features full"

# Default build (no cmake/nasm required)
docker run --rm -it -v $(pwd):/io -w /io messense/cargo-zigbuild \
  cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.28
```

The binary is generated at `target/x86_64-unknown-linux-gnu/release/veil`.

### Linux Packages (.deb / .rpm) and binary tarballs

Build Debian/Ubuntu (`.deb`) and Amazon Linux 2023 (`.rpm`) install packages, plus standalone glibc/musl binary tarballs, with all features enabled (`--features full`):

```bash
./packaging/scripts/build.sh
```

Outputs:

```
packaging/output/veil_<version>_<arch>.deb
packaging/output/veil-<version>-1.<arch>.rpm
packaging/output/veil-<version>-x86_64-unknown-linux-gnu.tar.gz
packaging/output/veil-<version>-x86_64-unknown-linux-musl.tar.gz
```

Build inside Docker ([docker/Dockerfile.glibc](../../docker/Dockerfile.glibc) for glibc 2.28-compatible binaries via `messense/cargo-zigbuild`, and [docker/Dockerfile.musl](../../docker/Dockerfile.musl) for musl):

```bash
# Build binaries and packages entirely inside Docker
./packaging/scripts/build.sh --docker
```

Install:

```bash
# Debian/Ubuntu
sudo dpkg -i packaging/output/veil_0.8.0_amd64.deb
sudo apt-get install -f
sudo systemctl enable --now veil

# Amazon Linux 2023
sudo dnf install -y packaging/output/veil-0.8.0-1.x86_64.rpm
sudo systemctl enable --now veil
```

Verify in Docker (both packages):

```bash
./packaging/scripts/test-install.sh
```

See [packaging/README.md](../../packaging/README.md) for details (Docker build, postinst behavior, troubleshooting).

> **Note**: `cmake` and `nasm` must be installed inside the container when building with `--features full` because the `http3` feature builds aws-lc-sys `libssl` (requires cmake) and `aws-lc-rs` uses assembly optimizations (requires nasm). The default build without `http3` does not need cmake.
>
> **`AWS_LC_SYS_NO_PREFIX` (per-target, B-47)**: for `http3` / `full` builds the value is set **only** in the `[env]` table of [`.cargo/config.toml`](../../.cargo/config.toml), using aws-lc-sys' target-suffixed variable names (`AWS_LC_SYS_NO_PREFIX_<triple_with_underscores>`):
> - **Linux → `1`**: quiche links the same *unprefixed* AWS-LC symbols as rustls (one shared `aws-lc-sys`).
> - **FreeBSD / Windows / macOS / OpenBSD → `0`**: quiche uses the external `boringssl-boring-crate` (`boring`), so `aws-lc-sys` must keep its symbol prefix to coexist. FreeBSD moved here in F-136 so that HTTP/3 certificate hot reload can use quiche's in-memory `SSL_CTX` API (`with_boring_ssl_ctx_builder`, only available with `boringssl-boring-crate`) under capsicum capability mode; sharing `aws-lc-sys` with the `boring` crate in one binary fails at link time with duplicate BoringSSL symbols.
>
> Cargo has no per-target environment mechanism (`[target.<triple>.env]` is silently ignored), and a `build.rs` cannot set env vars for its dependencies' build scripts (those run first, in separate processes). Do not set this variable in Dockerfiles or packaging scripts — the single source of truth is `.cargo/config.toml`.

> **Cargo features**: The complete list of available feature flags is defined in the [`[features]` section of `Cargo.toml`](../../Cargo.toml).
> Key notes:
> - **Default features**: `ktls`, `http2`, `mimalloc`
> - **`full`**: enables everything (`ktls`, `http2`, `http3`, `grpc-full`, `wasm`, `compression`, `cache`, `metrics`, `websocket`, `rate-limit`, `buffering`, `veil-alloc`). On unix the global allocator is **veil-alloc**; on Windows (where veil-alloc is not built) it falls back to mimalloc from the default features (or the system allocator with `--no-default-features`). Release builds for Windows and macOS use `full-windows` / `full-macos` instead (below)
> - **`full-freebsd` / `full-openbsd` / `full-netbsd`**: same feature set as `full`, but without mimalloc — all three use the **system allocator** (FreeBSD's `malloc(3)` is jemalloc already; bundling tikv-jemalloc on FreeBSD overrides libthr's malloc hooks and crashes the libc allocator, so the `jemalloc` feature is rejected at compile time on FreeBSD — B-89). All three include `wasm`; `full-openbsd`/`full-netbsd` run it through wasmtime's **Pulley interpreter** (B-52/B-55). All three use vendored TLS (rustls+aws_lc_rs on FreeBSD, rustls+ring on OpenBSD/NetBSD; quiche+bundled BoringSSL on all three). These sets are **architecture-independent** — the former `full-freebsd-aarch64` / `full-openbsd-aarch64` / `full-netbsd-aarch64` variants were byte-identical to their base sets and have been removed; use `--features full-freebsd` etc. on both x86_64 and aarch64. On NetBSD (any arch), FreeBSD aarch64, and OpenBSD aarch64, `wasm` builds against a vendored `third_party/wasmtime` (Pulley-only, see `third_party/wasmtime/README.veil.md`) instead of crates.io wasmtime, since crates.io wasmtime (36 / 40 alike) has no signal-handling support for these targets; every other platform is unaffected and keeps unmodified crates.io wasmtime. Cargo has no per-target default features, so the packaging scripts pass these explicitly with `--no-default-features` (`packaging/scripts/build-cross.sh --target freebsd`, `tools/qemu/bsd-vm.sh <os> <arch> build|e2e`). Plain `--features full` is unchanged.
> - **`full-windows` / `full-macos`** (F-184): same feature set as `full`, but with a different allocator instead of the unix-only veil-alloc: **mimalloc** on Windows, the **system allocator** (libmalloc) on macOS. Like the BSD sets, Cargo has no per-target default features, so `packaging/scripts/build-cross.sh --target windows|macos` (`docker/Dockerfile.windows` / `docker/Dockerfile.macos`) pass them with `--no-default-features`
> - **`full-container`** (F-144): same feature set as `full`, plus `epoll` — an explicit epoll-based readiness runtime (`veil_rt_reactor`, Linux only) instead of the default io_uring completion-based runtime (`veil_rt_uring`). Intended for container/orchestration environments (Docker, Kubernetes, gVisor) where seccomp profiles or the container runtime's syscall emulation frequently block or restrict io_uring. `default` is unchanged; build with `cargo build --features full-container` (or `--no-default-features --features full-container` if you also want to swap the allocator).
> - **`alloc-stats`** (F-165, diagnostic only, **off by default**): wraps the selected allocator (mimalloc/jemalloc/system) in a counting allocator and exposes `veil_alloc_allocs_total` / `veil_alloc_deallocs_total` / `veil_alloc_reallocs_total` / `veil_alloc_bytes_total` as Prometheus gauges, plus `veil_alloc_live_bytes` / `veil_alloc_live_allocs` (bytes and allocations currently alive, F-185) (needs `metrics` and `[prometheus] enabled = true`). Use it to measure **heap allocations per request** under load (`tools/perf/alloc_measure.sh`) and to check that repeated connections and reloads do not leak (`tools/perf/leak_check.sh`). Never enable it for production builds — every allocation pays an extra atomic increment.
> - **`veil-alloc`** (F-183): veil's own allocator (`src/veil_alloc/`, no dependencies beyond `libc`, unix only). It is one of the mutually exclusive allocator features; when the default `mimalloc` is also on (`cargo build --features full`), veil-alloc takes precedence. Tuning and measurements: [Memory Allocators](performance.md#memory-allocators-f-183)
> - **Allocator features** (`veil-alloc`, `mimalloc`, `jemalloc`, `system-allocator`) are mutually exclusive — enable at most one (to pick one other than the default `mimalloc`, use `--no-default-features`; if several are on anyway, the precedence is veil-alloc > mimalloc > jemalloc)
> - HTTP/3 is UDP-based and cannot be combined with kTLS


### Library artifacts (Rust source, C library, Python wheel)

The API server library ([library.md](library.md)) is packaged separately from the `veil` binaries, as a
single zip in `packaging/output/lib/`:

```bash
packaging/scripts/build-lib.sh                       # all kinds, in docker (manylinux_2_28, glibc >= 2.28)
packaging/scripts/build-lib.sh --kind c,python       # choose: rust, c, python
packaging/scripts/build-lib.sh --local --profile release   # build with the host toolchain (needs maturin)
```

Output: `veil-lib-<version>-<arch>-linux-gnu.zip` with `c/` (`libveil.so`, `libveil.a`, `veil.h`, pkg-config),
`python/` (abi3 wheel), `rust/` (source package and examples) and `docs/`.

### Build profiles

- **`cargo build --release`** uses Cargo's defaults (`lto = false`, `codegen-units = 16`).
  Measured on Linux x86_64 and FreeBSD aarch64, LTO makes **no measurable throughput
  difference** for veil's workload — the hot path is dominated by syscalls, TLS crypto and
  context switches, not cross-crate call overhead — so it is not worth slowing every
  development/E2E/perf build down (fat LTO took a FreeBSD incremental build from 26s to 4m25s).
  Release binaries keep their symbol table, which is what makes DTrace stacks and panic
  backtraces readable during performance work.
- **`cargo build --profile dist`** is the distribution profile: `inherits = "release"` plus
  `lto = "fat"`, `codegen-units = 1` and `strip = "symbols"`. Measured 36.8MB → 25.3MB
  (**-31%**). Only the packaging path uses it (`docker/Dockerfile.*`,
  `packaging/scripts/build.sh`, and `tools/qemu/bsd-vm.sh` via `CARGO_PROFILE=dist`).
- **`panic = "abort"` is deliberately not used.** `src/system.rs` wraps connection tasks in
  `catch_unwind` so a panic in one request is logged and the worker survives; aborting would
  turn a single malformed request into a full proxy outage, and `ConnectionGuard`'s `Drop`
  (connection accounting) would not run either.

### Versions and releases

The version is defined once, in `[workspace.package] version` of the root `Cargo.toml`. The root crate and
`crates/*` inherit it (`version.workspace = true`), and the Python wheel takes it from `crates/veil-python`
(`dynamic = ["version"]` in `pyproject.toml`). To change it:

```bash
tools/release/bump-version.sh 0.8.0
```

The script also updates the `veil-macros` dependency version, `Cargo.lock` of the workspace and of the crates
outside it (`examples/api/*`, `tools/*`), and the places in the docs that show the current version as an example.
Versions written as a measurement baseline (`docs/perf/`, `docs/guide/performance.md`, `tools/perf/`) are left as
they are. Running it twice with the same version changes nothing.

A release is a tag `vX.Y.Z` on a commit that is on `main`. The release workflow refuses to publish anything when
the tag and the workspace version differ.
