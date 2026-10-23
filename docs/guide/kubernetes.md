# Kubernetes (sidecar, Istio / Linkerd)

veil runs as a **sidecar container** next to an application in the same Pod and becomes the entry point of the
Service (F-196). It works unchanged inside an **Istio** or **Linkerd** mesh: the mesh proxies encrypt Pod-to-Pod
traffic with mTLS, and veil receives plaintext HTTP/1.1 and h2c inside the Pod. Manifests are in
[`deploy/kubernetes/`](../../deploy/kubernetes/README.md).

```
client Pod ──(mesh mTLS)──▶ [istio-proxy / linkerd-proxy] ─▶ veil :8080 (HTTP/1.1 + h2c) ─▶ app 127.0.0.1:9000
                                                             veil :8443 (TLS, certificate from a Secret)
```

## Helm

The chart installs veil as its own Deployment and Service (a gateway, a static site, a reverse proxy). For the
sidecar layout and the meshes, use the manifests below.

```bash
helm install veil oci://ghcr.io/aofusa/charts/veil --version <version>
kubectl port-forward svc/veil 8080:8080
curl http://127.0.0.1:8080/        # the page built into the image
```

The default configuration (rendered from the values into a ConfigMap) serves the image's `/var/www` over plaintext
HTTP/1.1 and h2c on port 8080. The pod runs as non-root (65532) with a read-only root filesystem, all capabilities
dropped and `RuntimeDefault` seccomp, and veil adds its own seccomp filter and Landlock. A changed configuration
restarts the pods (a checksum annotation). The chart is signed with cosign (keyless) like the images.

| Value | Default | |
|---|---|---|
| `image.repository` / `tag` / `digest` | `ghcr.io/aofusa/veil` / `musl-v<appVersion>` / — | `docker.io/aofusa/veil` also works |
| `config` / `existingConfigMap` | — | your own `config.toml` (`--set-file config=config.toml`) |
| `tls.secretName` | — | a `kubernetes.io/tls` Secret: adds the TLS listener (port 8443, Service port `service.tlsPort`) |
| `http3.enabled` | `false` | QUIC on UDP 8443, as a separate Service `<release>-veil-h3` (needs `tls.secretName`) |
| `metrics.enabled` / `metrics.serviceMonitor.enabled` | `false` | `/__metrics` and a Prometheus Operator ServiceMonitor |
| `grafanaDashboard.enabled` | `false` | the [example dashboard](observability.md#grafana-dashboard) as a ConfigMap for the Grafana sidecar |
| `extraVolumes` / `extraVolumeMounts` | — | your files, e.g. a ConfigMap mounted on `/var/www` |
| `autoscaling` / `podDisruptionBudget` / `networkPolicy` | off | |

Every value is described in [deploy/helm/veil/values.yaml](../../deploy/helm/veil/values.yaml). The Service has no
`appProtocol` (see the mesh notes below). The chart is tested on kind with `tools/release/helm-kind-test.sh`.

## Image

The published image `ghcr.io/aofusa/veil:latest` (musl; `:glibc` is equivalent, both for `linux/amd64` and
`linux/arm64`) works as is: it is the `full-container` build (epoll readiness reactor), which runs under the Pod's
seccomp profile (`RuntimeDefault` forbids io_uring) ([docker/README.md](../../docker/README.md)).

To build your own, use the same `full-container` feature set. The **musl** image is the main one; the glibc image is equivalent.

```bash
docker build -f docker/Dockerfile.musl  --build-arg CARGO_FEATURES=full-container -t veil:musl-k8s .
docker build -f docker/Dockerfile.glibc --build-arg CARGO_FEATURES=full-container -t veil:glibc-k8s .
```

## Layout

| Path | Contents |
|------|----------|
| `deploy/kubernetes/base` | Deployment (app + veil), Service, VDS reference control plane, test client, `config.toml`, VDS snapshot |
| `deploy/kubernetes/overlays/plain` | No mesh (namespace `veil-plain`) |
| `deploy/kubernetes/overlays/istio` | `istio-injection=enabled` namespace and a STRICT `PeerAuthentication` (`veil-istio`) |
| `deploy/kubernetes/overlays/linkerd` | `linkerd.io/inject: enabled` namespace (`veil-linkerd`) |

## Configuration in Kubernetes

| Concern | How |
|---------|-----|
| Config file | ConfigMap mounted at `/etc/veil/conf.d/config.toml` |
| Listeners | `[server] listen = "0.0.0.0:8443"` (TLS) and `listen_cleartext = "0.0.0.0:8080"` (plaintext HTTP/1.1 + h2c) with `enable_secure_redirect = false`, because the mesh already encrypts Pod-to-Pod traffic ([Listeners](configuration.md#listeners-listen-and-listen_cleartext)) |
| Per-environment differences | `VEIL_OVERRIDE_<n>` environment variables ([running.md](running.md#configuration-override-via-environment-variables-veil_override_n)); the overlays enable VDS with `VEIL_OVERRIDE_3='vds.enabled = true'` |
| TLS certificate | Secret mounted as a directory (never with `subPath`) and `[tls] auto_reload = true`; Secret updates are picked up through the `..data` symlink swap ([tls.md](tls.md#certificates-and-keys-in-containers)) |
| Dynamic routes | [VDS](vds.md): update the `veil-vds-snapshot` ConfigMap; the kubelet syncs it to the control plane within about a minute, the control plane pushes it to veil over a gRPC stream and veil applies it |
| Shutdown | veil stops accepting on SIGTERM, closes idle keep-alive connections at once and drains the rest within `graceful_shutdown_timeout_secs` (keep it below `terminationGracePeriodSeconds`) |
| Hardening | runs as non-root (65532) with a read-only root filesystem, all capabilities dropped and `RuntimeDefault` seccomp; veil's own seccomp/Landlock stay enabled |

## Mesh notes

- **Do not set `appProtocol` on the Service ports.** Linkerd treats `appProtocol: http` as HTTP/1.1 only, so
  h2c (HTTP/2 prior knowledge) on the same port breaks. Without it both meshes detect the protocol (Istio also
  uses the port name prefix `http` / `https`).
- Istio ≥ 1.2x and recent Linkerd inject their proxies as **native sidecars** (`initContainers` with
  `restartPolicy: Always`).
- veil forwards the identity headers the mesh adds (`X-Forwarded-Client-Cert` for Istio, `l5d-client-id` for
  Linkerd) to the application, so the application can still authorize by workload identity.
- Port 8443 (veil's own TLS) is opaque TLS to the mesh; use it for clients outside the mesh.
- The application container must handle SIGTERM itself; otherwise the Pod waits for the whole grace period even
  though veil has already stopped (the demo `traefik/whoami` runs as PID 1 and ignores SIGTERM).

## End-to-end test (kind)

```bash
tools/k8s/kind-e2e.sh musl all     # plain, Istio, Linkerd
tools/k8s/kind-e2e.sh glibc all
```

The script creates a kind cluster, loads the image, installs the mesh, deploys the overlay and checks:
connectivity (HTTP/1.1 and h2c), mTLS evidence (Istio XFCC SPIFFE ID / Linkerd `l5d-client-id`), rejection of
a client outside the mesh (Istio STRICT), veil's TLS from the Secret and its hot reload on Secret update,
`VEIL_OVERRIDE_<n>`, a route added through VDS (gRPC stream through the mesh, reconnect after the control plane restarts), and graceful shutdown after SIGTERM. It needs docker, kind,
kubectl, istioctl, linkerd and openssl. With snap-packaged docker the script keeps temporary files under
`target/tmp` (snap docker cannot read `/tmp`).

## veil as the mesh proxy

The setup above keeps the mesh's own proxy (Envoy for Istio, linkerd2-proxy for Linkerd) next to veil. veil can
also **replace that proxy** — sidecars, Istio gateways and the ambient waypoint, and Linkerd's sidecar — with
istiod and the Linkerd control plane unchanged and veil and stock proxies interoperating in one mesh. See
[Service mesh data plane](mesh.md).
