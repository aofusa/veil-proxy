# Requests and responses

## The request

A handler can receive the request by naming a parameter `req` or `request`, or by annotating it with
`veil.Request`:

```python
@veil.post("/echo")
def echo(req: veil.Request):
    return {
        "method": req.method,
        "path": req.path,
        "q": req.query_params,
        "ua": req.headers.get("user-agent"),
        "body": req.text(),
    }
```

| Member | Description |
|--------|-------------|
| `method`, `path`, `query` | HTTP method, path and raw query string |
| `headers` | case-insensitive read-only headers (`get`, `getlist`, `items`, `keys`, `in`) |
| `body`, `text()`, `json()` | the whole body as `bytes`, as text, parsed as JSON |
| `params`, `query_params` | path parameters, query parameters (the first value of a repeated key) |
| `client_ip` | the client address |
| `stream()` | iterate over the body as it arrives (routes with `stream_body=True`) |

Inside the server the object is a Rust-backed `veil._veil.NativeRequest` with the same attributes and methods;
an attribute becomes a Python object only when the handler reads it. `isinstance(req, veil.Request)` is true.

## Responses

Return a plain value (see {doc}`routing`) or build the response explicitly:

```python
@veil.get("/custom")
def custom():
    return veil.Response(b"hello", status=202, headers={"x-demo": "1"}, media_type="text/plain")

@veil.get("/json")
def as_json():
    return veil.JSONResponse({"ok": True}, status=200)
```

## Streaming (server to client)

```python
@veil.get("/stream")
def stream():                      # a generator (or async generator) is sent chunk by chunk as text/plain
    for i in range(3):
        yield f"line {i}\n"

@veil.get("/sse")
def sse():                         # Server-Sent Events
    return veil.EventSourceResponse({"event": "tick", "data": {"n": i}} for i in range(3))

@veil.get("/file")
def file():
    return veil.StreamingResponse(open("big.bin", "rb"), media_type="application/octet-stream",
                                  trailers=[("x-checksum", "...")])
```

SSE items are strings, JSON-serialisable values, or dicts with `event`, `data`, `id` and `retry`.
Streaming responses are iterated on a handler-pool thread (on an `offload=False` route the whole body is built
before it is sent). The iteration stops when the client disconnects.

## Streaming (client to server)

```python
@veil.post("/upload", stream_body=True)   # called before the body has arrived
def upload(req: veil.Request):
    return str(sum(len(c) for c in req.stream()))   # read in arrival order
```

## Server-Sent events and long-lived connections

See {doc}`websocket` for what to use instead of WebSocket endpoints.
