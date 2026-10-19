# Configuration

```python
veil.run(config_path=None, *, listen_cleartext=None, listen=None, tls_cert=None, tls_key=None,
         threads=None, overrides=None, openapi=True)
```

| Argument | Meaning |
|----------|---------|
| `config_path` | A veil `config.toml` to load (optional) |
| `listen_cleartext` | Plaintext HTTP/1.1 + h2c address (library default `127.0.0.1:8080`) |
| `listen` | TLS address; needs `tls_cert` and `tls_key` (HTTP/1.1, HTTP/2, HTTP/3) |
| `threads` | Number of network workers (`[server].threads`) |
| `overrides` | Keys to Python values, converted to TOML: `{"server.threads": 4, "api.max_body_size": 1048576}` |
| `openapi` | `False` disables `/openapi.json` and `/docs` |

The environment variables `VEIL_OVERRIDE_<n>` work as usual, e.g.
`VEIL_OVERRIDE_1='server.listen_cleartext = "0.0.0.0:8080"'`. The precedence is default, file, environment,
inline arguments. The keys are those of veil's `config.toml` (`examples/config.toml` in the repository).

`veil.Config` (`from_file`, `set`, `listen_cleartext`, `listen`, `tls`, `threads`, `to_toml`) can also be built
directly; it lives in the native extension, see {py:class}`veil._veil.Config`.

## Stopping

`veil.run()` blocks until Ctrl-C or SIGTERM, or until another thread calls {py:func}`veil.shutdown`; running
requests are finished (graceful shutdown). `veil.run()` releases the GIL while the server runs.

## Tests

`python -I tests/test_binding.py` runs the pure-Python unit tests (no native extension needed);
`tests/e2e.sh` runs the examples with curl after `maturin develop`.
