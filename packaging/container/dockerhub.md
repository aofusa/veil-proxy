# veil

A high-performance reverse proxy and web server: HTTP/1.1, HTTP/2, HTTP/3, gRPC and Proxy-Wasm filters,
sandboxed with seccomp and Landlock. Source and documentation: <https://github.com/aofusa/veil-proxy>.

This repository is a mirror of `ghcr.io/aofusa/veil`: the same tags, the same digests and the same
signatures.

## Try it

```sh
docker run --rm -p 8080:8080 aofusa/veil
```

Open <http://localhost:8080/>. The default configuration serves the page in `/var/www` over plaintext
HTTP/1.1 and h2c on port 8080 (no TLS certificate needed) and logs every request to standard output.

## Your files and your configuration

```sh
docker run --rm -p 8080:8080 -v "$PWD/site:/var/www:ro" aofusa/veil                       # your HTML
docker run --rm -p 8080:8080 -v "$PWD/config.toml:/etc/veil/conf.d/config.toml:ro" aofusa/veil  # your configuration
```

TLS and HTTP/3, reverse proxying and Kubernetes: see the
[container guide](https://github.com/aofusa/veil-proxy/blob/main/docs/guide/container.md).

## Tags

| Tag | Contents |
|-----|----------|
| `latest`, `musl`, `musl-vX.Y.Z` | static musl binary on `scratch` |
| `glibc`, `glibc-vX.Y.Z` | glibc binary on Wolfi (Chainguard `glibc-dynamic`) |

All tags are multi-arch (amd64, arm64). Versioned tags are never overwritten. Every image is scanned with
grype before it is published (no High or Critical vulnerability, including unfixed ones).

## Verify the signature

```sh
cosign verify aofusa/veil:musl-vX.Y.Z \
  --certificate-identity-regexp '^https://github.com/aofusa/veil-proxy/' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

Anonymous pulls from Docker Hub are rate limited; for CI, prefer `ghcr.io/aofusa/veil`.
