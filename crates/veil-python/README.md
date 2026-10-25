# veil-api

FastAPI-like Python bindings for the [veil](https://github.com/aofusa/veil-proxy) API server.

Accepting connections, TLS, HTTP/1.1, HTTP/2, HTTP/3, gRPC and OpenAPI serving are handled
by the Rust data plane (io_uring on Linux), so Python only contains your handler functions, written with
FastAPI-style decorators. The package is installed as `veil-api` and imported as `veil`.

## Install

```bash
pip install veil-api
pip install "veil-api[grpc]"      # adds grpcio, for veil.grpc.Server (generated grpc_tools.protoc code)
pip install pydantic              # optional: body validation and schema generation with models
```

Pre-built wheels are published for:

- Linux: manylinux and musllinux, x86_64 and aarch64
- macOS: arm64 and x86_64
- Windows: x86_64

All wheels are abi3, so one wheel works on every Python 3.9 or newer.
Linux uses io_uring; the other platforms use the platform's native readiness API (kqueue, WSAPoll).

## Quick start

```python
import veil

@veil.get("/ping", offload=False)
def ping():
    return "pong"

@veil.get("/hello/{name}")
def hello(name: str, greeting: str = "Hello"):   # name: path parameter, greeting: query parameter
    return {"message": f"{greeting}, {name}"}

@veil.post("/users", status_code=201,
           body_constraints={"name": {"type": "string", "minLength": 1, "maxLength": 50, "required": True},
                             "age": {"type": "integer", "minimum": 0, "maximum": 120, "required": True}})
def create_user(body: dict):
    return body

veil.run(listen_cleartext="127.0.0.1:8000")      # stops gracefully on Ctrl-C / SIGTERM
```

You can also create an application object, as in FastAPI:

```python
app = veil.App(title="Demo", version="1.0")

@app.get("/")
def index():
    return {"ok": True}

app.run(listen_cleartext="127.0.0.1:8000")
```

The server also serves `/openapi.json` and Swagger UI at `/docs` (disable with `run(openapi=False)`).

## Handlers at a glance

- **Arguments** are bound from the signature: a parameter named `req`/`request` (or annotated `veil.Request`)
  receives the request; a name that appears in the path is a path parameter (converted by its `int`/`float`/`bool`/`str`
  annotation); a pydantic `BaseModel`, `dict`/`list` annotation or the name `body` receives the JSON body;
  anything else is a query parameter. Missing or invalid values produce a FastAPI-style 422 response.
- **Return values**: `dict`/`list`/numbers become JSON, `str` is `text/plain`, `bytes` is
  `application/octet-stream`, `None` is 204; `veil.Response`, `veil.JSONResponse`, `(body, status)` and pydantic
  models are also accepted. `raise veil.HTTPException(404, "not found")` produces `{"detail": "not found"}`.
  An unhandled exception is printed to stderr and answered with 500.
- **Streaming**: generators (sync or async), `veil.StreamingResponse`, `veil.EventSourceResponse` (Server-Sent
  Events) and `@veil.post(..., stream_body=True)` for streamed request bodies.
- **gRPC**: `veil.grpc.unary_unary` and friends (no dependencies), or `veil.grpc.Server` with grpcio-generated code.
- **WebSocket**: `@app.websocket("/ws")` with the Starlette / FastAPI API (`veil.WebSocket`, `accept`,
  `receive_text`, `send_text`, `iter_text`, `veil.WebSocketDisconnect`, `veil.WebSocketEndpoint`); frames are
  handled in Rust. WebSocket routes are listed in OpenAPI. Other upgrades: `veil.Response.switching_protocols`.
- **async and lifespan**: `async def` handlers run on a persistent asyncio event loop (uvloop when installed);
  `veil.run_in_threadpool`, `veil.App(lifespan=...)`, `@app.on_event("startup")`, `app.state`.
- **L4**: `@veil.l4_tcp` and `@veil.l4_udp` for raw TCP and UDP listeners.
- **Configuration**: `veil.run(config_path=None, *, listen_cleartext=None, listen=None, tls_cert=None, tls_key=None,
  threads=None, overrides=None, openapi=True)`. `overrides` maps config keys to Python values, for example
  `{"server.threads": 4}`. Use `listen` with `tls_cert` and `tls_key` for TLS (HTTP/2 and HTTP/3).
- **Access log**: one line per request is written to standard output by default (`[access_log] enabled = true`,
  `format = "text"`). Turn it off with `overrides={"access_log.enabled": False}`.

## Threads and the GIL

Python handlers run while holding the GIL.

- By default (`offload=True`) a handler runs on veil's handler thread pool (`[api] handler_threads`), so the
  I/O worker threads never wait for the GIL and keep serving connections. `async def` handlers run on the
  persistent event loop (`[api] python_event_loops`, default 1); the workers queue calls for it without taking
  the GIL. `async_mode="pool"` keeps the previous behaviour (one event loop per pool thread).
- With `offload=False` the handler runs directly on the I/O worker thread. Use it only for trivially cheap
  handlers: blocking I/O or long GIL holds stall every connection that worker owns.
- Pure Python handlers are limited by the GIL; for CPU-bound work consider a multi-process deployment.
- Streaming responses are iterated on a handler pool thread. On an `offload=False` route the whole body is built
  before it is sent.

## Documentation

Full documentation (quick start, routing, requests and responses, dependency injection, long-lived connections, gRPC,
configuration and the API reference) is on Read the Docs: <https://veil-api.readthedocs.io>.

Source code, examples and issue tracker: <https://github.com/aofusa/veil-proxy>.

## License

Apache-2.0 OR MIT
