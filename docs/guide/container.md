# Container image

veil is published as a multi-arch (amd64, arm64) container image at **`ghcr.io/aofusa/veil`**.

| Tag | Contents |
|-----|----------|
| `latest`, `musl`, `musl-vX.Y.Z` | static musl binary on `scratch` |
| `glibc`, `glibc-vX.Y.Z` | glibc binary on Wolfi ([Chainguard `glibc-dynamic`](https://images.chainguard.dev/directory/image/glibc-dynamic/overview); distroless up to v0.8.3) |

Both run under Docker's default seccomp profile and Kubernetes `RuntimeDefault`: the binary is the
`full-container` build (epoll readiness reactor; see [Building](building.md)). Versioned tags are never
overwritten. Every image is scanned with grype before it is published, and a release is not published
if any image has a High or Critical vulnerability, including ones that have no fixed version yet.

## Try it

```sh
docker run --rm -p 8080:8080 ghcr.io/aofusa/veil
```

Open <http://localhost:8080/>. The image's built-in configuration serves the built-in page from
`/var/www` over plaintext HTTP/1.1 and h2c on port 8080, with no TLS certificate needed. It logs every
request to standard output (`docker logs`). The page links to `/quickstart.html`, which has the same
steps as this guide.

```sh
curl -i http://localhost:8080/
curl --http2-prior-knowledge -I http://localhost:8080/
```

## Quickstart: your files and your configuration

| Path in the image | What it is | Replace it with |
|-------------------|------------|-----------------|
| `/var/www/` | the built-in page | `-v "$PWD/site:/var/www:ro"` |
| `/etc/veil/conf.d/config.toml` | the built-in configuration | `-v "$PWD/config.toml:/etc/veil/conf.d/config.toml:ro"` |
| `/etc/veil/ssl/` | (not in the image) TLS certificate and key | `-v "$PWD/ssl:/etc/veil/ssl:ro"` |

### Serve your own HTML

```sh
mkdir -p site && echo '<h1>Hello from veil</h1>' > site/index.html
docker run --rm -p 8080:8080 -v "$PWD/site:/var/www:ro" ghcr.io/aofusa/veil
```

### Use your own config.toml

Start from the built-in configuration
([`packaging/container/config.toml`](../../packaging/container/config.toml)) and change what you need.
Every key is described in [Configuration](configuration.md) and
[`examples/config.toml`](../../examples/config.toml).

```sh
docker run --rm -p 8080:8080 \
  -v "$PWD/config.toml:/etc/veil/conf.d/config.toml:ro" \
  -v "$PWD/site:/var/www:ro" \
  ghcr.io/aofusa/veil

# validate a configuration without starting the server
docker run --rm -v "$PWD/config.toml:/etc/veil/conf.d/config.toml:ro" \
  ghcr.io/aofusa/veil /veil -t -c /etc/veil/conf.d/config.toml
```

A reverse proxy in front of another container. Routes are evaluated in order and the first match wins
([Routing](routing.md)), so put `/api/` before `/`:

```toml
[server]
listen_cleartext = "0.0.0.0:8080"
enable_secure_redirect = false

[[route]]
[route.conditions]
path = "/api/"
[route.action]
type = "Proxy"
url = "http://backend:3000"

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/var/www/"
```

### TLS and HTTP/3

```toml
[server]
listen = "0.0.0.0:443"              # TLS: HTTP/1.1 and HTTP/2 (ALPN)
listen_cleartext = "0.0.0.0:8080"   # with enable_secure_redirect = true (default) this answers 301 to https

[tls]
cert_path = "/etc/veil/ssl/cert.pem"
key_path = "/etc/veil/ssl/key.pem"

[http3]
enabled = true                      # UDP on the same address as [server].listen
```

```sh
docker run --rm -p 8080:8080 -p 443:443/tcp -p 443:443/udp \
  -v "$PWD/config.toml:/etc/veil/conf.d/config.toml:ro" \
  -v "$PWD/ssl:/etc/veil/ssl:ro" -v "$PWD/site:/var/www:ro" \
  ghcr.io/aofusa/veil
```

See [TLS](tls.md) for certificate reload and ACME, and [Protocols](protocols.md) for HTTP/3.

## Operating the container

- **Reload** the configuration and certificates without dropping connections:
  `docker kill -s HUP <container>` ([Running](running.md)).
- **Stop** gracefully with `docker stop` (SIGTERM).
- **Privileges**: veil starts as root, binds its listeners and then drops to the `nonroot` user
  (uid/gid 65532). It installs its own seccomp filter and Landlock rules. Mounted files must be readable
  by uid 65532. Directories outside the built-in Landlock list must be added to
  `[security] landlock_read_paths` ([Security](security.md)).
- **Read-only root filesystem** works. Give `/var/cache/veil` and `/var/tmp/veil` a `tmpfs` if you use the
  cache or request buffering.
- **Logs**: the application log and the access log go to standard output, errors to standard error
  ([Observability](observability.md)).

## Kubernetes

Mount the configuration from a ConfigMap at `/etc/veil/conf.d/config.toml` and certificates from a Secret
at `/etc/veil/ssl`. Manifests, sidecar use and the service mesh data plane are covered in
[Kubernetes](kubernetes.md).

## Building the image yourself

```sh
docker build -f docker/Dockerfile.musl --build-arg CARGO_FEATURES=full-container -t veil:musl .
tools/release/image-smoke.sh veil:musl defaults
```

[`docker/README.md`](../../docker/README.md) lists all Dockerfiles and build arguments (including the
io_uring build with an epoll fallback).
