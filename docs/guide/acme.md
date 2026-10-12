# ACME: Automatic Certificates (Let's Encrypt)

[← Documentation index](README.md) · [日本語](ja/acme.md)

veil can obtain and renew its TLS certificate by itself using the ACME protocol (RFC 8555) with the **HTTP-01**
challenge, for example from Let's Encrypt. No external tool (certbot) is needed, and certificates are swapped in
by the existing [certificate hot reload](tls.md#tls-certificate-hot-reload) with no downtime.

> **Requires**: build with `--features acme` (included in `--features full` and the other `full*` sets).

- The ACME client runs on a **dedicated thread**; the data plane is never touched.
- Only HTTP-01 is supported (no DNS-01, so **no wildcard certificates**). The names must resolve to this server and
  port 80 must reach it.

## Configuration

```toml
[server]
listen = "0.0.0.0:443"
http2_enabled = true

[tls]
cert_path = "/etc/veil/tls/cert.pem"      # the issued certificate chain is written here
key_path  = "/etc/veil/tls/key.pem"       # and the private key here (mode 0600)

[acme]
enabled = true
domains = ["example.com", "www.example.com"]
contact = ["mailto:admin@example.com"]
accept_terms_of_service = true
storage_dir = "/var/lib/veil/acme"
```

| Key | Default | Description |
|-----|---------|-------------|
| `enabled` | `false` | turn ACME on |
| `directory_url` | `"https://acme-v02.api.letsencrypt.org/directory"` | ACME directory URL. For testing use Let's Encrypt **staging**: `https://acme-staging-v02.api.letsencrypt.org/directory` |
| `contact` | `[]` | contact URLs for the account, e.g. `["mailto:admin@example.com"]` |
| `domains` | `[]` | names to put in the certificate (required, at least one; the first becomes the CN and all are SANs) |
| `storage_dir` | `"/var/lib/veil/acme"` | where the account key and state are kept; created with mode 0700 and must be writable |
| `accept_terms_of_service` | `false` | you agree to the CA's terms of service. **Must be `true`**, otherwise veil refuses to start |
| `challenge_listen` | `"0.0.0.0:80"` | address of the plaintext HTTP-01 responder; empty = do not start one (see below) |
| `renew_before_days` | `30` | renew when the certificate expires within this many days |
| `check_interval_secs` | `43200` | how often to check the certificate (12 h; minimum 60) |
| `directory_ca_cert` | `""` | extra CA (PEM) to trust for the ACME directory — for test servers such as Pebble |
| `order_timeout_secs` | `300` | time limit for one issuance (minimum 30) |

`[acme]` also needs `[tls] cert_path` and `key_path`: they are where the certificate is written and loaded from.
`[acme]` is read at startup; changing it needs a restart (not SIGHUP). Like every key it can be set with
`VEIL_OVERRIDE_<n>` (see [Running](running.md#configuration-override-via-environment-variables-veil_override_n)).

## How it works

1. **Startup (before privilege drop and the sandbox).** If `[tls] cert_path`/`key_path` do not exist, veil writes a
   **temporary self-signed certificate** for `domains` there, so the TLS listener can start immediately, and marks it
   as a placeholder in `storage_dir`. Then the HTTP-01 responder is bound on `challenge_listen`
   (port 80 needs root or `CAP_NET_BIND_SERVICE`; binding happens before veil drops privileges).
   Certificate hot reload is switched on automatically, whatever `[tls] auto_reload` says.
2. **Issuance (ACME thread).** While the certificate is a placeholder, unreadable, or expires within
   `renew_before_days`, the thread creates (or loads) the ACME account key (`storage_dir/account.key`, mode 0600),
   places an order for `domains`, publishes the HTTP-01 token on the responder, waits for the CA to validate it,
   generates a new certificate key and CSR, finalizes, and downloads the chain.
3. **Installation.** The private key and then the certificate chain are written to `key_path` and `cert_path`
   **atomically** (temporary file in the same directory, `fsync`, `rename`; key mode 0600), the placeholder marker is
   removed, and a certificate reload is triggered immediately. New TLS handshakes (HTTP/1.1, HTTP/2 and HTTP/3) use the
   new certificate; existing connections are not disturbed.
4. **Renewal.** Every `check_interval_secs` the certificate's expiry is checked and the steps above repeat when it is within
   `renew_before_days`. A failed attempt is retried with exponential backoff (5 s up to 1 h) and the current certificate
   keeps being served.

### The HTTP-01 responder

The responder is a tiny plaintext HTTP/1.1 server on its own thread. It answers
`GET /.well-known/acme-challenge/<token>` with the key authorization, and answers every other request with a
`301` redirect to `https://<Host><path>`, so port 80 can double as the HTTP→HTTPS redirect (you do not need
`[server] http` for it; do not bind both to the same port). It handles at most 64 connections at a time.

If something else already serves port 80 (a load balancer, another proxy, a Kubernetes Ingress), set
`challenge_listen = ""` or a high port such as `"0.0.0.0:8080"` and forward
`/.well-known/acme-challenge/` from port 80 to it.

## Requirements and operations

- The names in `domains` must point at this server, and **TCP port 80 must be reachable from the internet**
  (the CA connects to port 80 of each name, regardless of `challenge_listen`).
- `cert_path`'s and `key_path`'s directories and `storage_dir` must be writable by the user veil runs as after
  dropping privileges. Make `storage_dir` persistent (a volume in containers), otherwise a new account is created on every
  start and rate limits apply.
- If the **Landlock** sandbox is enabled, add the certificate directory and `storage_dir` to
  `landlock_write_paths` (and the certificate directory to `landlock_read_paths`). seccomp must allow the usual
  network syscalls (it does by default). For containers, see
  [TLS in containers](tls.md#certificates-and-keys-in-containers).
- Use the staging directory (`https://acme-staging-v02.api.letsencrypt.org/directory`) while setting up: the
  production CA enforces strict rate limits. Staging certificates are not trusted by browsers. veil only issues again when
  the certificate is a placeholder or near expiry, so when you switch `directory_url` to production, stop veil, delete
  the staging `cert_path` and `key_path` files, and start it again (a temporary certificate is written and the
  production certificate is issued).
- Back up `storage_dir/account.key` if you want to keep the same account.
- Logs use the `[ACME]` prefix: `wrote a temporary self-signed certificate`, `managing a certificate for ...`,
  `certificate installed`, `issuance failed (retry in ...)`.

## Testing with Pebble

[Pebble](https://github.com/letsencrypt/pebble) is Let's Encrypt's ACME test server. The repository contains an
end-to-end test that starts Pebble in Docker, lets veil obtain a certificate over HTTP-01 and checks that veil moves from the
temporary certificate to the issued one without restart:

```bash
cargo build --features full
tests/acme/pebble_e2e.sh [path/to/veil]      # needs docker, openssl, curl
```

It uses `directory_url = "https://localhost:14000/dir"`, `directory_ca_cert` (Pebble's minica) and
`challenge_listen = "[::]:5002"`, and verifies the temporary certificate, the issuance, the content served over HTTPS, the
account key / placeholder state and the key file mode (0600).

## Alternative: certbot

ACME inside veil is optional. You can keep an external client and let veil pick up the files:

```toml
[tls]
cert_path = "/etc/letsencrypt/live/example.com/fullchain.pem"
key_path  = "/etc/letsencrypt/live/example.com/privkey.pem"
auto_reload = true
```

```bash
certbot renew --deploy-hook "touch /etc/letsencrypt/live/example.com/fullchain.pem"
```

See [TLS certificate hot reload](tls.md#tls-certificate-hot-reload). Use one method or the other for a given
certificate; do not enable `[acme]` on a certificate that certbot also manages.
