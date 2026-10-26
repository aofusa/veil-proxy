# Production

The defaults of `veil.run()` are meant for development: the server listens on `127.0.0.1:8080` only, every request
is written to the access log on standard output, and OpenAPI with Swagger UI is served at `/openapi.json` and
`/docs`. For production, put the settings in a configuration file and load it:

```python
veil.run(config_path="config.toml")
```

```toml
[server]
listen_cleartext = "0.0.0.0:8080"    # reachable from other hosts and containers
enable_secure_redirect = false
graceful_shutdown_timeout_secs = 30

[logging]
format = "json"

[access_log]
enabled = false

[api]
openapi_enabled = false
compression = true
max_body_size = 1048576

[prometheus]
enabled = true
path = "/metrics"
allowed_ips = ["127.0.0.1", "10.0.0.0/8"]
```

The same keys can be given as `veil.run(overrides={"access_log.enabled": False, ...})` or per deployment as
`VEIL_OVERRIDE_1='api.openapi_enabled = false'` (environment overrides win over the file). The complete example is
`examples/api/production/` in the repository, and the full guide (listeners and TLS, threads, logging, compression,
static files, file descriptors, shutdown, metrics) is `docs/guide/library-production.md`.

## Handlers

| Handler | Runs on | Use it for |
|---|---|---|
| `def f()` (default `offload=True`) | the handler pool (`[api] handler_threads`, default twice the CPUs, at least 4) | ordinary handlers, blocking libraries |
| `def f()` with `offload=False` | the network worker, directly | tiny handlers that never block (a health check, a constant) |
| `async def f()` | the persistent event loop (`[api] python_event_loops`, default 1) | handlers that wait for I/O with async libraries |
| `await veil.run_in_threadpool(g, ...)` inside `async def` | the thread pool | blocking or CPU-heavy calls from `async def` handlers |

- Never block inside `async def`: a blocking call stops every coroutine of the loop.
- Install `uvloop` for a faster event loop (`python_event_loop = "auto"` uses it when present).
- CPU-bound Python code is limited by the GIL. Run one process per host (or container) with the default threads,
  and add processes on separate ports or containers behind a load balancer if one is not enough. Several processes
  on the same port are not recommended.
- `python_event_loop_queue` (65536) bounds the calls that may wait for a loop; beyond it requests get 503.

## Health checks

```python
@veil.get("/healthz", offload=False)
def healthz():
    return "ok"
```

Keep the health check free of I/O. Add a readiness route that checks the database if the load balancer needs one.

## Shutdown

SIGTERM stops accepting connections, closes WebSockets with code 1001 and waits up to
`[server] graceful_shutdown_timeout_secs` for requests in flight. `lifespan` and `on_shutdown` functions run after
the server stopped (see {doc}`async`). On Windows, call `veil.shutdown()`.

## File descriptors

Each connection and WebSocket uses one file descriptor. veil raises the soft limit to the hard limit; raise the hard
limit where the process starts (`LimitNOFILE=1048576` in systemd, `docker run --ulimit nofile=1048576:1048576`).
