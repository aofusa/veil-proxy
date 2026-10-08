# References & Logos

[← Documentation index](README.md) · [日本語](ja/references.md)

## References

### Core Libraries

- [monoio](https://github.com/bytedance/monoio): io_uring-based async runtime
- [rustls](https://github.com/rustls/rustls): Pure Rust TLS implementation
- [kTLS (custom)](https://docs.kernel.org/networking/tls.html): Custom kernel TLS module implemented in `src/ktls.rs` and `src/ktls_rustls.rs`
- [httparse](https://crates.io/crates/httparse): Fast HTTP parser
- [quiche](https://github.com/cloudflare/quiche): Cloudflare's QUIC/HTTP/3 implementation

### Performance

- [mimalloc](https://github.com/microsoft/mimalloc): Fast general-purpose memory allocator
- [matchit](https://crates.io/crates/matchit): Fast Radix Tree router
- [ftlog](https://crates.io/crates/ftlog): High-performance async logging library
- [memchr](https://crates.io/crates/memchr): SIMD-optimized string search
- [Linux Huge Pages](https://docs.kernel.org/admin-guide/mm/hugetlbpage.html): Large OS Pages configuration guide

### Monitoring

- [prometheus](https://crates.io/crates/prometheus): Prometheus metrics library

### CLI & Concurrency

- [clap](https://crates.io/crates/clap): Command line argument parser
- [arc-swap](https://crates.io/crates/arc-swap): Lock-free Arc swapping (for config hot reload)
- [ctrlc](https://crates.io/crates/ctrlc): Signal handling (for Graceful Shutdown)
- [signal-hook](https://crates.io/crates/signal-hook): SIGHUP handling (for Graceful Reload)
- [core_affinity](https://crates.io/crates/core_affinity): CPU affinity configuration

### Kernel Features

- [Linux Kernel TLS](https://docs.kernel.org/networking/tls.html): kTLS documentation
- [io_uring](https://kernel.dk/io_uring.pdf): io_uring design document
- [SO_REUSEPORT](https://lwn.net/Articles/542629/): Port sharing and load balancing

### Security

- [systemd.exec](https://www.freedesktop.org/software/systemd/man/systemd.exec.html): systemd security settings
- [seccomp](https://docs.kernel.org/userspace-api/seccomp_filter.html): Seccomp BPF filter
- [Landlock](https://docs.kernel.org/userspace-api/landlock.html): Filesystem sandbox
- [io_uring Security](https://www.kernel.org/doc/html/latest/userspace-api/io_uring.html): io_uring security considerations
- [bubblewrap](https://github.com/containers/bubblewrap): Unprivileged sandboxing tool

### WASM Extensions

- [Proxy-Wasm](https://github.com/proxy-wasm/spec): Proxy-Wasm ABI Specification
- [Wasmtime](https://wasmtime.dev/): WebAssembly Runtime
- [proxy-wasm-rust-sdk](https://github.com/proxy-wasm/proxy-wasm-rust-sdk): Rust SDK

## Logos

<table align="center">
  <tr>
    <th align="center">Main Logo (WebP)</th>
    <th align="center">Alternative Logo (SVG)</th>
    <th align="center">Logo Text (SVG)</th>
  </tr>
  <tr>
    <td align="center">
      <img src="../images/veil_logo.webp" alt="Veil Main Logo" width="200" />
    </td>
    <td align="center">
      <img src="../images/veil_logo_alternative.svg" alt="Veil Alternative Logo" width="200" />
    </td>
    <td align="center">
      <img src="../images/veil_logo_text.svg" alt="Veil Logo Text" width="200" />
    </td>
  </tr>
</table>
