# Veil Documentation

[← README](../../README.md) · [日本語](ja/README.md)

User guides split out of the top-level README. Each page keeps the original section text.

| Topic | Guide |
|-------|-------|
| Feature overview | [features.md](features.md) |
| Platforms & runtime backends | [platforms.md](platforms.md) |
| Building & packaging | [building.md](building.md) |
| Install, configuration file, validation, reload & shutdown | [running.md](running.md) |
| Container image (try it, your own config and files) | [container.md](container.md) |
| Configuration reference | [configuration.md](configuration.md) |
| TLS, kTLS & certificate reload | [tls.md](tls.md) |
| Routing, redirects & headers | [routing.md](routing.md) |
| Load balancing, health checks & resilience | [load-balancing.md](load-balancing.md) |
| L4 stream proxy | [l4-proxy.md](l4-proxy.md) |
| HTTP/2, HTTP/3 & WebSocket | [protocols.md](protocols.md) |
| Compression, cache & buffering | [compression-cache.md](compression-cache.md) |
| WASM extensions | [wasm.md](wasm.md) |
| API server library (Rust, C, Python) | [library.md](library.md) |
| VDS: dynamic routes from a control plane | [vds.md](vds.md) ([schema](vds-schema.json)) |
| ACME: automatic certificates | [acme.md](acme.md) |
| Kubernetes (Helm chart, sidecar, Istio / Linkerd) | [kubernetes.md](kubernetes.md) |
| Service mesh data plane (veil as Envoy / linkerd2-proxy) | [mesh.md](mesh.md) |
| Metrics, Grafana dashboard, tracing & logging | [observability.md](observability.md) |
| Admin & cache purge APIs | [admin-api.md](admin-api.md) |
| Security & sandboxing | [security.md](security.md) |
| Performance tuning & benchmarks | [performance.md](performance.md) |
| Testing (developers) | [testing.md](testing.md) |
| Migrating from nginx | [migration-nginx.md](migration-nginx.md) |
| Migrating from Envoy | [migration-envoy.md](migration-envoy.md) |
| References & logos | [references.md](references.md) |

Other documents:

- [examples/config.toml](../../examples/config.toml) — commented reference of every configuration key
- [docs/perf/README.md](../perf/README.md) — latest benchmark results
- [docs/history/](../history/) — design history and lessons learned (developers)
- [AGENTS.md](../../AGENTS.md) — contributor guide
