# VDS: Dynamic Routes and Upstreams from a Control Plane

[← Documentation index](README.md) · [日本語](ja/vds.md)

VDS (Veil Discovery Service) lets a **control plane** push the routes and upstreams of a running veil. It is a
minimal subset of Envoy's xDS (REST-JSON, State-of-the-World) with the same semantics: a version, a nonce,
long polling, and ACK/NACK. When the control plane publishes a new snapshot, veil validates it, swaps it in with
its normal hot-reload path (zero downtime) and acknowledges it. A snapshot that fails validation is rejected
(NACK) and the previous configuration keeps serving.

> **Requires**: build with `--features vds` (included in `--features full` and the other `full*` sets).
> The machine-readable protocol schema is [vds-schema.json](vds-schema.json).

- veil talks to the control plane on a **dedicated thread**; the data plane (worker event loops) is never touched.
- Applying a snapshot goes through exactly the same validation as a SIGHUP reload, so a bad snapshot cannot
  take the proxy down.
- The control plane is any HTTP server implementing the protocol below. A small reference implementation,
  `tools/vds/vds_server.py`, ships with veil.

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
| `server` | `""` | control plane base URL: `http://host:port[/base]` or `https://...` (required when enabled; the client posts to `{server}/v1/discovery`) |
| `node_id` | `""` | identity sent to the control plane; empty = the host name |
| `cluster` | `""` | cluster / group label sent to the control plane |
| `token_file` | `""` | file containing a Bearer token; read again for **every** request so it can be rotated; empty = no `Authorization` header |
| `ca_cert` | `""` | extra trusted CA (PEM) for `https` servers; empty = the built-in web PKI roots only |
| `client_cert` / `client_key` | `""` | client certificate and key (PEM) for mutual TLS; used only when both are set |
| `poll_timeout_secs` | `30` | how long the control plane may hold a request when nothing changed (sent as `poll_timeout_secs`); the HTTP timeout is this value + 15 s |
| `retry_initial_ms` | `500` | first retry delay after an error (doubles each time) |
| `retry_max_ms` | `30000` | maximum retry delay |
| `max_response_bytes` | `16777216` | reject responses larger than this (16 MiB) |
| `max_resources` | `10000` | reject snapshots with more resources than this |

Because VDS is just configuration, it also accepts `VEIL_OVERRIDE_<n>` and `--override`
(see [Running](running.md#configuration-override-via-environment-variables-veil_override_n)), for example
`VEIL_OVERRIDE_1='vds.server = "http://cp:18000"'`.

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
    "user_agent_version": "0.7.0"
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

### Long polling, 304, ACK and NACK

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

## Reference control plane

`tools/vds/vds_server.py` (Python 3.11+, standard library only) serves a snapshot file and long-polls veil nodes:

```bash
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

The server watches the file; when its content changes (the version is the first 16 hex digits of the SHA-256), it
answers the held requests. A snapshot that fails to parse as TOML is not published and the previous one stays.
Endpoints: `POST /v1/discovery` (the protocol), `GET /v1/status` (per-node acknowledged version and last NACK,
as JSON) and `GET /healthz`. Run veil with:

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
          image: python:3.12-alpine
          command: ["python3", "-I", "/opt/vds/vds_server.py", "--snapshot", "/etc/vds/snapshot.toml",
                    "--listen", "0.0.0.0:18000", "--token-file", "/etc/veil/vds/token"]
          volumeMounts:
            - {name: code, mountPath: /opt/vds, readOnly: true}       # ConfigMap holding vds_server.py
            - {name: snapshot, mountPath: /etc/vds, readOnly: true}
            - {name: token, mountPath: /etc/veil/vds, readOnly: true} # Secret
      volumes:
        - {name: code, configMap: {name: veil-vds-server}}
        - {name: snapshot, configMap: {name: veil-vds-snapshot}}
        - {name: token, secret: {secretName: veil-vds-token}}
---
apiVersion: v1
kind: Service
metadata: {name: veil-vds}
spec:
  selector: {app: veil-vds}
  ports: [{name: http-vds, port: 18000, targetPort: 18000}]
```

Do not mount the snapshot with `subPath` (the kubelet never updates such mounts). In the veil container, set
`[vds] server = "http://veil-vds:18000"` and `token_file` to the same Secret. A ready-to-apply example (sidecar
Deployment, VDS control plane, Istio/Linkerd overlays) is in [`deploy/kubernetes/`](../../deploy/kubernetes/).
`kubectl edit configmap veil-vds-snapshot` then changes the routes of every veil within seconds to a minute.

## Troubleshooting

| Symptom | Check |
|---------|-------|
| nothing happens | `[vds] enabled = true`, a valid `server` URL, and the `[VDS]` lines in the log (`subscribing to ...`) |
| `rejected snapshot` warnings | the message is the validation error; fix the snapshot (the previous configuration is still serving) |
| `control plane returned 401` | `token_file` content does not match the server's token |
| TLS errors | `ca_cert` for a private CA, the server name in the certificate, and `client_cert`/`client_key` for mTLS |
| snapshot ignored in a reload | it is not; the overlay is re-applied on every SIGHUP. A `-o`/`VEIL_OVERRIDE_<n>` on the same key overrides it |
