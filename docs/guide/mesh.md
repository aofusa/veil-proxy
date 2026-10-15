# Service mesh data plane (veil as Envoy / linkerd2-proxy)

[← Documentation index](README.md) · [日本語](ja/mesh.md)

veil can **replace the mesh proxy itself**: the Envoy that Istio injects (sidecars, ingress / egress gateways and the
ambient waypoint) and the linkerd2-proxy that Linkerd injects. The control planes (istiod, Linkerd's destination /
identity / policy controllers) are used unchanged, and veil-proxied Pods interoperate with stock-proxied Pods in the
same mesh (F-215 to F-225). This is different from [running veil next to the mesh proxy](kubernetes.md), where the
mesh keeps its own proxy.

```
                 istiod / Linkerd control plane (unchanged)
                    │ xDS (ADS, SDS)          │ Destination / Policy / Identity gRPC
   ┌────────────────▼───────┐        ┌────────▼───────────────┐
   │ pilot-agent + veil     │◀──────▶│ linkerd2-proxy-identity │
   │ (as /usr/local/bin/envoy)│ mTLS │ + veil (as linkerd2-proxy)│
   └────────────────────────┘        └─────────────────────────┘
```

## How it works

- **Separate engine.** The mesh data plane is its own module (`src/mesh/`, features `mesh`, `mesh-istio`,
  `mesh-linkerd`). The existing proxy paths are not changed (the only shared piece is the HTTP/2 server driver,
  which is generic over the per-stream service and monomorphized), so a build without the mesh features is the
  same binary as before, and `default` / `full*` do not include the mesh.
- **Same binary, chosen by how it is started.** When `argv[0]` is `envoy` (Istio's pilot-agent starts
  `/usr/local/bin/envoy`), veil reads the Envoy bootstrap and speaks Delta ADS / SDS over pilot-agent's Unix
  sockets. When `argv[0]` is `linkerd2-proxy`, veil reads the `LINKERD2_PROXY_*` environment variables and
  talks to Linkerd's control plane over gRPC.
- **One model.** Envoy's xDS and Linkerd's policies are translated into the same intermediate model (listeners,
  filter chains, routes, clusters, endpoints, RBAC). Updates replace resources with `ArcSwap`; the data plane only
  reads them. Requests on long-lived connections pick up the new configuration from the next request.
- **Transparent interception** uses the mesh's own iptables (istio-init / istio-cni, linkerd-init) and
  `SO_ORIGINAL_DST`. Linux only.

## Images

Both images use the static **musl** build of veil and keep everything else from the stock proxy image
(pilot-agent and its templates; linkerd2-proxy-init and linkerd2-proxy-identity):

```bash
docker build -f docker/Dockerfile.musl --build-arg CARGO_FEATURES=mesh-istio,mesh-linkerd,epoll,wasm -t veil:musl-mesh .
docker build -f docker/Dockerfile.mesh-istio   -t veil.local/istio-proxy:1.31.1 .        # replaces /usr/local/bin/envoy
docker build -f docker/Dockerfile.mesh-linkerd -t veil.local/linkerd-proxy:edge-26.10.1 . # replaces /usr/lib/linkerd/linkerd2-proxy
```

`epoll` selects the readiness reactor because the default container seccomp profile rejects io_uring. `wasm` is needed
for Istio's WasmPlugin. The versions tested are Istio 1.31.1 and Linkerd edge-26.10.1 (proxy v2.370.0).

## Istio

| What | How |
|------|-----|
| Sidecar | Pod annotation `sidecar.istio.io/proxyImage: veil.local/istio-proxy:1.31.1` (the name must contain `/`, otherwise the injection template ignores it), or `istioctl install --set values.global.proxy.image=...` for the whole mesh |
| Ingress / egress gateway (Istio Gateway) | gateway Deployment with `inject.istio.io/templates: gateway` and the same annotation |
| Gateway API gateway, ambient waypoint | `spec.infrastructure.parametersRef` pointing to a ConfigMap whose `deployment` overlay sets the `istio-proxy` container image |

Supported (checked with `tools/k8s/mesh-e2e.sh`, stock / veil / mixed):

- Sidecar inbound / outbound (virtualInbound / virtualOutbound, original destination, protocol detection), HTTP/1.1,
  HTTP/2 (h2c, gRPC), TCP, passthrough, `PERMISSIVE` / `STRICT` mTLS (Istio's ALPN, SAN checks, XFCC,
  inbound from `127.0.0.6`), peer metadata exchange (Istio's telemetry labels).
- Traffic management tasks: fault injection (abort, delay), timeouts, retries, header changes, traffic shifting,
  rewrites and subsets, mirroring, circuit breaking and outlier detection, consistent hashing.
- Security: `AuthorizationPolicy` (HTTP and TCP), `RequestAuthentication` (JWT with inline JWKS).
- Telemetry: Istio's standard metrics (`istio_requests_total` etc. through pilot-agent's merged metrics),
  Telemetry API access logs (Envoy's default format), distributed tracing (Zipkin / B3).
- WasmPlugin (ECDS → veil's Proxy-Wasm, including C++ SDK modules; `FAIL_CLOSE` returns 503 when the module traps).
- Gateways: HTTP, HTTPS `SIMPLE` / `MUTUAL` with `credentialName`, TLS `PASSTHROUGH` (SNI), egress gateway,
  Gateway API `HTTPRoute`.
- Ambient: veil as the **waypoint** (HBONE: HTTP/2 CONNECT over mTLS on 15008, Envoy's internal listeners,
  `filter_chain_matcher`), L7 `AuthorizationPolicy` and `HTTPRoute` traffic routing at the waypoint. ztunnel stays
  stock.
- `istioctl proxy-status` / `proxy-config` (listeners, routes, clusters, endpoints, secrets) through Envoy's
  admin API on 15000 (`/config_dump`, `/clusters?format=json`, `/stats`, `/ready`, `/drain_listeners`,
  `/quitquitquit`).

## Linkerd

| What | How |
|------|-----|
| Sidecar | Pod annotations `config.linkerd.io/proxy-image: veil.local/linkerd-proxy` and `config.linkerd.io/proxy-version: edge-26.10.1`, or `linkerd inject --proxy-image veil.local/linkerd-proxy --proxy-version edge-26.10.1` |

The init container (`linkerd-init`) runs from the same image, so tag the veil image with the stock version.

Supported (checked with `tools/k8s/linkerd-e2e.sh`, stock / veil / mixed):

- Identity: CSR from `linkerd2-proxy-identity`, `Identity.Certify`, renewal at 70% of the certificate lifetime;
  `/ready` turns 200 once the certificate arrives.
- Discovery and policy: `OutboundPolicies.Watch` (protocol detection, `HTTPRoute` / `GRPCRoute` matches, header
  filters, redirects, failure injection, weighted backends, timeouts, retries, failure accrual),
  `Destination.Get` (endpoints, identities, HTTP/2 hints, opaque transport), `InboundServerPolicies.WatchPort`
  (`Server`, `AuthorizationPolicy`, `MeshTLSAuthentication`, route-level authorization,
  the default inbound policy).
- Proxy-to-proxy protocol (needed to mix with stock proxies): mTLS with DNS-SAN identities and no ALPN,
  HTTP/1.1 → HTTP/2 upgrade with `l5d-orig-proto`, opaque transport (`transport.l5d.io/v1` ALPN and transport
  header), `l5d-client-id` to the application, `l5d-proxy-error` on proxy-generated responses.
- Tasks: Automatic mTLS, Restricting Access To Services, Per-Route Policy, Retries, Timeouts, Traffic Split,
  Circuit Breaking (failure accrual); certificate renewal with short-lived certificates; control plane restarts.
- Observability: `/metrics` with linkerd2-proxy's names and labels (`request_total`, `response_total`,
  `response_latency_ms`, `tcp_*`, `control_identity_cert_expiration_timestamp_seconds`), so `linkerd viz stat` and
  `edges` work; tap on 4190 (`linkerd viz tap` / `top`), accepted only from the tap service identity.
  emojivoto and booksapp run with every Pod on veil.

## Environment variables

| Variable | Meaning |
|----------|---------|
| `VEIL_MESH_LOG` | log level of the mesh data plane (`trace` … `error`); overrides Envoy's `-l` and `LINKERD2_PROXY_LOG` |
| `VEIL_MESH_KTLS` | `1` enables kTLS for mesh mTLS (off by default: nodes without the `tls` module would log a fallback per connection) |
| `VEIL_MESH_XDS_RECORD` | directory where veil saves every Delta ADS response it receives (`NNNN-<type>.pb`; SDS responses, which carry keys, are not saved). Used to capture istiod's xDS as test fixtures. Set it through `proxy.istio.io/config: '{"proxyMetadata": {"VEIL_MESH_XDS_RECORD": "/etc/istio/proxy/xds"}}'` (the root filesystem is read-only) |

## Differences and limitations

- Envoy filters veil does not implement are rejected (NACK) unless the filter is marked optional; the previous
  configuration stays in effect, as with Envoy. `config_dump` contains what istioctl reads, not every field.
- X.509 v1 client certificates are rejected (webpki); Envoy (BoringSSL) accepts them. Certificates issued by the
  meshes are v3.
- ztunnel (ambient L4) is not replaced. Linkerd's multicluster gateways (`authority_override`) and the legacy
  `ServiceProfile` routes (`GetProfile`) are not used; Linkerd's current `HTTPRoute`-based policies are.
- Tap events are emitted when a request completes (the three events of a request carry the same timing).

## Performance

Measured on kind with fortio (`tools/k8s/mesh-bench.sh`, client and server sidecars, alternating rounds). See
[docs/perf/README.md](../perf/README.md#service-mesh) for the full table.

## Tests

```bash
tools/k8s/mesh-e2e.sh    <stock|veil|mixed> <smoke|tasks|gateway|telemetry|wasm|ambient|observe|chaos|all>   # Istio
tools/k8s/linkerd-e2e.sh <stock|veil|mixed> <smoke|tasks|ops|certs|viz|all>                                  # Linkerd
tools/k8s/mesh-bench.sh  [rounds] [duration]                                     # Istio sidecar benchmark
tools/k8s/linkerd-bench.sh [rounds] [duration]                                   # Linkerd sidecar benchmark
tools/k8s/mesh-gateway-conformance.sh <stock|veil>                               # Gateway API conformance (GATEWAY-HTTP)
tools/k8s/mesh-fixtures.sh                                                       # capture istiod's xDS into tests/fixtures/istio/<version>/
tools/k8s/mesh-syscalls.sh [image] [connections]                                 # syscalls per intercepted connection (strace)
cargo test --test mesh_istio_fixtures --features mesh-istio                      # translate the captured xDS (no NACK, key settings)
```

`observe` runs bookinfo with Prometheus, Kiali, Grafana and Jaeger (Istio's samples/addons) and checks the Kiali
graph (every edge with the mTLS lock), the Istio dashboards' panel queries, a trace across all four services,
`istioctl analyze`, Pod start-up time and SIGTERM draining. `chaos` restarts and stops istiod under load, deletes
destination Pods and kills the proxy process.

`stock` runs the same checks with the stock proxies (to validate the script), `mixed` makes some workloads veil and
the rest stock so traffic crosses both ways. They need kind, kubectl, istioctl and linkerd.
