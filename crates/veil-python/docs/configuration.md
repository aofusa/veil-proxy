# Configuration

```python
veil.run(config_path=None, *, listen_cleartext=None, listen=None, tls_cert=None, tls_key=None,
         threads=None, overrides=None, openapi=True, listeners=None, http3=None)
```

| Argument | Meaning |
|----------|---------|
| `config_path` | A veil `config.toml` to load (optional) |
| `listen_cleartext` | Plaintext HTTP/1.1 + h2c address (library default `127.0.0.1:8080`) |
| `listen` | TLS address; needs `tls_cert` and `tls_key` (HTTP/1.1, HTTP/2, HTTP/3) |
| `http3` | `True` also serves HTTP/3 on the UDP port of `listen`; a string sets another UDP address |
| `listeners` | A list of `veil.Listener` (or dicts with the same keys), one per port; see below |
| `threads` | Number of network workers (`[server].threads`) |
| `overrides` | Keys to Python values, converted to TOML: `{"server.threads": 4, "api.max_body_size": 1048576}` |
| `openapi` | `False` disables `/openapi.json` and `/docs` |

The environment variables `VEIL_OVERRIDE_<n>` work as usual, e.g.
`VEIL_OVERRIDE_1='server.listen_cleartext = "0.0.0.0:8080"'`. The precedence is default, file, environment,
inline arguments. The keys are those of veil's `config.toml` (`examples/config.toml` in the repository).

## Several listeners

To open several ports with different protocols, pass one `veil.Listener` per port. Each becomes a
`[[listener]]` entry of veil's `config.toml` (see "Multiple listeners" in veil's configuration guide):

```python
veil.run(
    tls_cert="cert.pem", tls_key="key.pem",
    listeners=[
        veil.Listener("0.0.0.0:8080", name="plain"),                    # HTTP/1.1 + h2c
        veil.Listener("0.0.0.0:8081", tls=True, name="h1tls", http2=False),  # TLS, HTTP/1.1 only
        veil.Listener("0.0.0.0:8082", name="h2c", allow_http=False),    # h2c only
        veil.Listener("0.0.0.0:8443", tls=True, name="https", http3=True),   # HTTP/1.1 + HTTP/2 + HTTP/3
    ],
)
```

`veil.Listener(listen, tls=False, *, name, http2, http3, http3_listen, allow_http, allow_h2c, secure_redirect,
redirect_to, threads, unix_socket_permissions)`. `http2` and `http3` apply to TLS listeners, `allow_http`,
`allow_h2c`, `secure_redirect` and `redirect_to` to plaintext ones. Once `listeners` is given, the library default
`127.0.0.1:8080` is not opened unless `listen_cleartext` is also given. `veil.Config.add_listener(...)` takes the
same arguments.

## Access log

Every request is logged to standard output by default, one `text` line per request
(`timestamp=… type=access method=… path=… status=… duration_ms=…`). Lines are flushed every
`access_log.flush_interval_ms` (default 1000 ms). Turn it off with `overrides={"access_log.enabled": False}`
(or `VEIL_OVERRIDE_1='access_log.enabled = false'`); switch to JSON with `{"access_log.format": "json"}`.

`veil.Config` (`from_file`, `set`, `listen_cleartext`, `listen`, `tls`, `add_listener`, `threads`, `to_toml`) can also be built
directly; it lives in the native extension, see {py:class}`veil._veil.Config`.

## Stopping

`veil.run()` blocks until Ctrl-C or SIGTERM, or until another thread calls {py:func}`veil.shutdown`; running
requests are finished (graceful shutdown). `veil.run()` releases the GIL while the server runs.

## Tests

`python -I tests/test_binding.py` runs the pure-Python unit tests (no native extension needed);
`tests/e2e.sh` runs the examples with curl after `maturin develop`.
