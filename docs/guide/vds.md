# VDS: Dynamic Routes and Upstreams from a Control Plane

[← Documentation index](README.md) · [日本語](ja/vds.md)

VDS (Veil Discovery Service) lets a **control plane** push the routes and upstreams of a running veil. It is a
minimal subset of Envoy's xDS (State-of-the-World) with the same semantics: a version, a nonce and ACK/NACK,
carried over a gRPC bidirectional stream (default) or REST-JSON long polling. When the control plane publishes a new snapshot, veil validates it, swaps it in with
its normal hot-reload path (zero downtime) and acknowledges it. A snapshot that fails validation is rejected
(NACK) and the previous configuration keeps serving.

> **Requires**: build with `--features vds` (included in `--features full` and the other `full*` sets).
> The machine-readable protocol schema is [vds-schema.json](vds-schema.json).

- veil talks to the control plane on a **dedicated thread**; the data plane (worker event loops) is never touched.
- Applying a snapshot goes through exactly the same validation as a SIGHUP reload, so a bad snapshot cannot
  take the proxy down.
- The control plane is any server implementing the protocol below. Reference implementations ship with veil:
  `tools/vds/server` (Rust, gRPC + REST) and `tools/vds/vds_server.py` (Python, REST).

## Configuration

`[vds]` is read at startup. It is not applied by SIGHUP; restart veil to change it.

```toml
[vds]
enabled = true
server = "https://cp.example.com:18000"
node_id = "edge-1"
cluster = "prod"
token_file = "/etc/veil/vds/token"
```

| Key | Default | Description |
|-----|---------|-------------|
| `enabled` | `false` | start the VDS client thread |
| `server` | `""` | control plane base URL: `http://host:port[/base]` or `https://...` (required when enabled, unless `servers` is set) |
| `servers` | `[]` | several control planes for redundancy (F-211). Takes precedence over `server`. When a connection or stream fails veil tries the next one right away; after a full round it waits with the retry backoff |
| `transport` | `"grpc"` | `"grpc"`: bidirectional stream (h2c or TLS with ALPN h2); `"rest"`: long polling of `{server}/v1/discovery` |
| `keepalive_secs` | `30` | TCP keepalive of the gRPC connection (`0` = off) |
| `node_id` | `""` | identity sent to the control plane; empty = the host name |
| `cluster` | `""` | cluster / group label sent to the control plane |
| `token_file` | `""` | file containing a Bearer token; read again for every request (REST) or connection (gRPC) so it can be rotated; empty = no `Authorization` header |
| `ca_cert` | `""` | extra trusted CA (PEM) for `https` servers; empty = the built-in web PKI roots only |
| `client_cert` / `client_key` | `""` | client certificate and key (PEM) for mutual TLS; used only when both are set |
| `poll_timeout_secs` | `30` | REST: how long the control plane may hold a request when nothing changed (sent as `poll_timeout_secs`); the HTTP timeout is this value + 15 s |
| `retry_initial_ms` | `500` | first retry delay after an error (doubles each time) |
| `retry_max_ms` | `30000` | maximum retry delay |
| `max_response_bytes` | `16777216` | reject responses (gRPC: messages) larger than this (16 MiB) |
| `max_resources` | `10000` | reject snapshots with more resources than this |
| `initial_fetch_timeout_ms` | `0` | at startup, wait up to this long for the first snapshot before opening the listeners (`0` = do not wait). When it expires veil starts with the file configuration. Useful for a Kubernetes sidecar that must not serve stale routes (F-211) |

Because VDS is just configuration, it also accepts `VEIL_OVERRIDE_<n>` and `--override`
(see [Running](running.md#configuration-override-via-environment-variables-veil_override_n)), for example
`VEIL_OVERRIDE_1='vds.server = "http://cp:18000"'`.

## Status: metrics and admin API

The state of the client (F-211) is exported to Prometheus (`[prometheus] enabled = true`) when VDS is enabled:

| Metric | Type | Meaning |
|--------|------|---------|
| `veil_proxy_vds_connected` | gauge | `1` while a control plane answers, `0` otherwise |
| `veil_proxy_vds_applied_info{version_info}` | gauge | always `1`; the label is the snapshot version being served |
| `veil_proxy_vds_updates_total{result="applied"\|"rejected"}` | counter | snapshots applied (ACK) and rejected (NACK) |
| `veil_proxy_vds_last_update_timestamp_seconds` | gauge | Unix time of the last snapshot applied or rejected |

The [admin API](admin-api.md) returns the same as JSON at `GET {path_prefix}/vds` (default `/__admin/vds`):

```json
{"enabled":true,"connected":true,"server":"http://cp-1:18000","version_info":"3f2a9c1d04b7e612",
 "last_error":null,"updates_acked":4,"updates_nacked":1,"last_update_unix":1791700000}
```

`last_error` is the reason of the last rejected snapshot (or the last connection error). Without VDS the endpoint
returns `{"enabled":false}`.

## What a snapshot controls

A snapshot is the **complete** set of routes and upstreams (State-of-the-World): each time it replaces the previous one,
it is not a patch.

- `[[route]]` entries are replaced as a whole. Routes written in `config.toml` are **not merged**: while the control plane
  has delivered a snapshot that contains routes, the routes of `config.toml` are not used. Keep a fallback in the
  snapshot if you need one.
- `[upstreams.<name>]` tables are replaced as a whole in the same way.
- A snapshot that contains only routes leaves `upstreams` from `config.toml` untouched, and vice versa.
- An **empty snapshot** (`"resources": []`) means "no routes and no upstreams": it empties both.
- Until the first snapshot arrives (or while the control plane is unreachable), veil keeps using
  `config.toml` and the last applied snapshot.

**Cannot be changed by VDS** (they need a restart): `[server]` listeners, `[tls]`, `[[l4]]`, `[security]` and every
other section. Only `route` and `upstreams` are replaced.

### Precedence

```
config.toml  <  VDS snapshot  <  VEIL_OVERRIDE_<n> environment  <  --override on the command line
```

A higher layer wins. For example, `VEIL_OVERRIDE_1='route.0.action.path = "/srv"'` modifies the route that VDS
delivered. Overlay and overrides are applied again on every reload, so a SIGHUP keeps the delivered snapshot.

## Protocol

Two transports carry the same messages and semantics (`[vds] transport`):

- **gRPC** (default): one long-lived bidirectional stream. The control plane pushes a new snapshot as soon as it
  changes; there is no polling.
- **REST**: JSON long polling (below). Use it for control planes that cannot speak HTTP/2.

### gRPC (default)

The service is defined in [`proto/veil/vds/v1/vds.proto`](../../proto/veil/vds/v1/vds.proto):

```proto
service VeilDiscoveryService {
  rpc StreamSnapshots(stream DiscoveryRequest) returns (stream DiscoveryResponse);
}
message DiscoveryRequest  { string version_info = 1; Node node = 2; string type_url = 3;
                            string response_nonce = 4; ErrorDetail error_detail = 5; }
message DiscoveryResponse { string version_info = 1; string nonce = 2; string type_url = 3;
                            repeated Resource resources = 4; }
message Resource { string type_url = 1; string name = 2; string json = 3; }
```

veil opens `POST {server}/veil.vds.v1.VeilDiscoveryService/StreamSnapshots` over h2c (`http://`) or TLS with ALPN
`h2` (`https://`) on its VDS thread (veil's own HTTP/2 client; no tokio, no tonic), with `authorization: Bearer
<token>` when `token_file` is set (read on every connection). The fields mean exactly what they mean in REST; a
`Resource` is one element of the REST `resources` array with `@type` moved to `type_url`, `name` to `name`, and
the rest of the object serialized as JSON in `json`.

1. On connect veil sends a request with `version_info = ""` (or the last applied version after a reconnect).
2. The control plane sends a `DiscoveryResponse` whenever the snapshot differs from what the node has (and was not
   rejected).
3. veil applies it and sends the next request on the same stream: **ACK** (new `version_info` and the response's
   `nonce`) or **NACK** (old `version_info`, the `nonce` and `error_detail`).
4. If the stream ends, fails or carries a non-zero `grpc-status`, veil reconnects with exponential backoff and
   resumes from its last applied version. `keepalive_secs` (TCP keepalive) detects a vanished control plane during
   long quiet periods.

### REST

All requests are `POST {server}/v1/discovery` with `Content-Type: application/json`. If `token_file` is set, each
request carries `Authorization: Bearer <token>`.

### DiscoveryRequest

```json
{
  "version_info": "3b1f9c2a77d04e51",
  "node": {
    "id": "edge-1",
    "cluster": "prod",
    "user_agent_name": "veil",
    "user_agent_version": "0.8.0"
  },
  "type_url": "type.veil.dev/veil.v1.Snapshot",
  "response_nonce": "17",
  "poll_timeout_secs": 30
}
```

| Field | Meaning |
|-------|---------|
| `version_info` | the last **successfully applied** version; empty (`""`) before the first snapshot |
| `node` | `id` (`node_id`), `cluster`, and the veil version (`user_agent_*`) |
| `type_url` | always `type.veil.dev/veil.v1.Snapshot` |
| `response_nonce` | the `nonce` of the last response this client handled (applied or rejected); empty before the first |
| `poll_timeout_secs` | the longest the server may hold this request |
| `error_detail` | present only in a NACK (see below): `{"code": 3, "message": "..."}` |

### DiscoveryResponse

```json
{
  "version_info": "9d02ce8e1b56a3f0",
  "nonce": "18",
  "type_url": "type.veil.dev/veil.v1.Snapshot",
  "resources": [
    {
      "@type": "type.veil.dev/veil.v1.Upstream",
      "name": "api",
      "servers": ["http://10.0.0.11:8080", "http://10.0.0.12:8080"]
    },
    {
      "@type": "type.veil.dev/veil.v1.Route",
      "conditions": { "path": "/api/*" },
      "action": { "type": "Proxy", "upstream": "api" }
    }
  ]
}
```

| Field | Meaning |
|-------|---------|
| `version_info` | an opaque version string of this snapshot (for example a content hash) |
| `nonce` | an opaque token identifying this response; echoed back in `response_nonce` |
| `type_url` | optional; if present it must be `type.veil.dev/veil.v1.Snapshot`, otherwise veil rejects the response |
| `resources` | the resources; each object has an `@type` |

### Resource types

| `@type` | Content |
|---------|---------|
| `type.veil.dev/veil.v1.Route` | one `[[route]]` entry, as JSON in the same shape as the TOML (`conditions`, `action`, `security`, ...). A `name` field is ignored. |
| `type.veil.dev/veil.v1.Upstream` | one `[upstreams.<name>]` table. The required `name` is the table name; the remaining fields are the table (`servers`, `algorithm`, ...). |

JSON `null` values are dropped (TOML has no null). Any other `@type`, a non-object resource, an upstream without
`name`, or more than `max_resources` resources rejects the whole snapshot. The order of `Route` resources is the
order of evaluation (first match wins), the same as in the file.

Everything a route or upstream can contain in `config.toml` is allowed — see the
[Configuration](configuration.md), [Routing](routing.md) and [Load balancing](load-balancing.md) guides and
[examples/config.toml](../../examples/config.toml).

#### Long polling, 304, ACK and NACK

1. veil sends a request with its current `version_info`.
2. If the control plane has nothing newer, it **holds** the request until something changes or until
   `poll_timeout_secs` passes, then answers **`304 Not Modified`** (empty body). veil immediately asks again.
3. If there is a different version, the control plane answers **`200`** with a `DiscoveryResponse`.
4. veil validates the snapshot (resource checks, then the full configuration validation and reload):
   - **ACK** — success: the next request carries the new `version_info` and the response's `nonce`.
   - **NACK** — failure: the previous configuration stays in effect, and the next request carries the **old**
     `version_info`, the response's `nonce` and `error_detail` (sent once). A control plane should not resend the
     rejected version until its content changes (the reference server does this).
5. Any other status or a network error: veil logs a warning, waits with exponential backoff
   (`retry_initial_ms` doubling up to `retry_max_ms`) and tries again. The last good configuration keeps serving.

## Security

- Use `https://` for anything outside a trusted network, and set `ca_cert` when the control plane uses a private CA.
- `client_cert` + `client_key` enable **mutual TLS**, so the control plane can authenticate each veil node.
- `token_file` carries a Bearer token. Put it in a file (not an environment variable; see
  [TLS in containers](tls.md#certificates-and-keys-in-containers)); it is re-read for each request, so rotating the
  file needs no restart.
- A snapshot can route traffic anywhere, so treat the control plane like the configuration itself: restrict who can
  publish, and authenticate the channel.
- If the sandbox is on (`[security]`), the VDS thread runs inside it, so the files above must be readable
  (`landlock_read_paths`).

## Reference control planes

Two reference implementations ship with veil. Both serve a snapshot file and keep the per-node status:

| | `tools/vds/server` (Rust) | `tools/vds/vds_server.py` (Python) |
|---|---|---|
| transports | gRPC **and** REST on one port (HTTP/1.1, h2c, TLS) | REST only |
| run | `cargo run --release --manifest-path tools/vds/server/Cargo.toml -- ...` or the `docker/Dockerfile.vds-server` image | `python3 tools/vds/vds_server.py ...` (3.11+, standard library only) |

```bash
veil-vds-server --snapshot snapshot.toml --listen 0.0.0.0:18000 \
    [--token-file token] [--hold 30] [--tls-cert cert.pem --tls-key key.pem]
python3 tools/vds/vds_server.py --snapshot snapshot.toml --listen 0.0.0.0:18000 \
    [--token-file token] [--hold 30]
```

The snapshot is a TOML file containing only `[[route]]` and `[upstreams.<name>]`, written exactly like
`config.toml`:

```toml
[upstreams.api]
servers = ["http://10.0.0.11:8080", "http://10.0.0.12:8080"]

[[route]]
[route.conditions]
path = "/api/*"
[route.action]
type = "Proxy"
upstream = "api"
```

The servers watch the file; when its content changes (the version is the first 16 hex digits of the SHA-256), they
push it to gRPC streams and answer the held REST requests. A snapshot that fails to parse as TOML is not published and the previous one stays.
Endpoints: the gRPC service (Rust only), `POST /v1/discovery`, `GET /v1/status` (per-node acknowledged version,
last NACK and transport, as JSON) and `GET /healthz`. Run veil with:

```toml
[vds]
enabled = true
server = "http://127.0.0.1:18000"
```

It is a reference for testing and small deployments. For production, implement the protocol in your own control plane
(see [vds-schema.json](vds-schema.json)) and secure it as described above.

## Kubernetes

Mount the snapshot as a **ConfigMap** into a small control-plane Pod and point every veil at its Service. Editing
the ConfigMap reaches all nodes within the ConfigMap propagation delay (typically under a minute), with no rollout.

```yaml
apiVersion: v1
kind: ConfigMap
metadata: {name: veil-vds-snapshot}
data:
  snapshot.toml: |
    [[route]]
    [route.conditions]
    path = "/"
    [route.action]
    type = "Proxy"
    url = "http://127.0.0.1:9000"
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: veil-vds}
spec:
  replicas: 1
  selector: {matchLabels: {app: veil-vds}}
  template:
    metadata: {labels: {app: veil-vds}}
    spec:
      containers:
        - name: vds
          image: veil-vds-server:latest   # docker/Dockerfile.vds-server
          args: ["--snapshot", "/etc/vds/snapshot.toml", "--listen", "0.0.0.0:18000",
                 "--token-file", "/etc/veil/vds/token"]
          volumeMounts:
            - {name: snapshot, mountPath: /etc/vds, readOnly: true}
            - {name: token, mountPath: /etc/veil/vds, readOnly: true} # Secret
      volumes:
        - {name: snapshot, configMap: {name: veil-vds-snapshot}}
        - {name: token, secret: {secretName: veil-vds-token}}
---
apiVersion: v1
kind: Service
metadata: {name: veil-vds}
spec:
  selector: {app: veil-vds}
  ports: [{name: grpc-vds, port: 18000, targetPort: 18000}]   # Istio: grpc- prefix; Linkerd detects HTTP/2
```

Do not mount the snapshot with `subPath` (the kubelet never updates such mounts). In the veil container, set
`[vds] server = "http://veil-vds:18000"` and `token_file` to the same Secret. A ready-to-apply example (sidecar
Deployment, VDS control plane, Istio/Linkerd overlays) is in [`deploy/kubernetes/`](../../deploy/kubernetes/).
`kubectl edit configmap veil-vds-snapshot` then changes the routes of every veil within seconds to a minute.

## Troubleshooting

| Symptom | Check |
|---------|-------|
| nothing happens | `[vds] enabled = true`, a valid `server` URL, and the `[VDS]` lines in the log (`subscribing to ...`) |
| which version is running / why was it rejected | `GET /__admin/vds` or the `veil_proxy_vds_*` metrics |
| startup takes `initial_fetch_timeout_ms` | no control plane answered in time (`no snapshot within initial_fetch_timeout_ms` in the log); veil then started with the file configuration |
| `rejected snapshot` warnings | the message is the validation error; fix the snapshot (the previous configuration is still serving) |
| `control plane returned 401` / `grpc-status 16` | `token_file` content does not match the server's token |
| `stream ... failed` with `HTTP 404` / `did not negotiate h2` | the control plane does not speak gRPC: set `transport = "rest"` (for example with `tools/vds/vds_server.py`) |
| TLS errors | `ca_cert` for a private CA, the server name in the certificate, and `client_cert`/`client_key` for mTLS |
| snapshot ignored in a reload | it is not; the overlay is re-applied on every SIGHUP. A `-o`/`VEIL_OVERRIDE_<n>` on the same key overrides it |
