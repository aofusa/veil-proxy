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

## Forms and file uploads

The names are FastAPI's, and no `python-multipart` is needed: the body is parsed in Rust.

```python
from typing import Annotated, Optional

import veil
from veil import File, Form, UploadFile


@veil.post("/profile")
async def profile(
    name: str = Form(),                       # required text field
    age: int = Form(0),                       # converted like a query parameter
    tags: list[str] = Form([]),               # repeated fields
    avatar: UploadFile = File(),              # required file
    banner: Optional[UploadFile] = None,      # optional file (UploadFile needs no File())
):
    data = await avatar.read()
    return {"name": name, "file": avatar.filename, "type": avatar.content_type, "size": avatar.size}


@veil.post("/files")
def files(docs: list[UploadFile], note: Annotated[str, Form()] = ""):
    return [d.filename for d in docs]


@veil.post("/raw")
def raw(doc: bytes = File()):                  # the content as bytes
    return len(doc)


@veil.post("/login")                          # application/x-www-form-urlencoded uses the same Form()
def login(user: str = Form(), remember: bool = Form(False)):
    return {"user": user}
```

- The form is read **before** the handler runs. Text fields and files up to `[api] multipart_spool_threshold`
  (1 MiB) stay in memory; larger files are written by Rust to an anonymous temporary file in `[api] upload_tmp_dir`
  and `UploadFile.file` reads it, so a 1 GB upload does not grow the process. The temporary file disappears when it
  is closed (`await upload.close()`, or when the object is collected).
- `UploadFile` has `filename`, `content_type`, `headers` (a dict, lower-case names), `size`, `file` (a binary file
  object positioned at the start), `async` `read(size=-1)` / `write` / `seek` / `close`, and `read_bytes()` for
  synchronous handlers.
- A missing field or file gives 422 with `loc = ["body", name]` (`Form(alias="other")` changes the name); a
  malformed body gives 400 and one over the `[api] multipart_*` limits 413 (`{"detail": "..."}`).
- OpenAPI describes the body as `multipart/form-data` (files are `{"type": "string", "format": "binary"}`) or as
  `application/x-www-form-urlencoded` when there are no files, so the Swagger UI can send files.

## Server-Sent events and long-lived connections

WebSocket endpoints and other protocol upgrades (`veil.Response.switching_protocols`) are described in {doc}`websocket`.
