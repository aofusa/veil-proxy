# Quickstart

## Install

```bash
pip install veil-api
```

To build from source (for development):

```bash
python3 -m venv .venv && . .venv/bin/activate
pip install maturin
cd crates/veil-python
maturin develop --release        # install into the venv
maturin build --release          # build a wheel (target/wheels/)
```

## A first server

```python
import veil

@veil.get("/ping", offload=False)
def ping():
    return "pong"

@veil.get("/hello/{name}")
def hello(name: str, greeting: str = "Hello"):       # name: path, greeting: query (converted by annotation)
    return {"message": f"{greeting}, {name}"}

@veil.post("/users", status_code=201,
           body_constraints={"name": {"type": "string", "minLength": 1, "maxLength": 50, "required": True},
                             "age": {"type": "integer", "minimum": 0, "maximum": 120, "required": True}})
def create_user(body: dict):
    return body

veil.run(listen_cleartext="127.0.0.1:8000")
```

Like FastAPI you can also create an application object: `app = veil.App(title=..., version=...)`, then use
`@app.get(...)` and `app.run()`. OpenAPI is served at `/openapi.json` and Swagger UI at `/docs`
(`run(openapi=False)` disables them).

## Coming from FastAPI

| FastAPI | veil |
|---------|------|
| `app = FastAPI(title=..., version=...)` | `app = veil.App(title=..., version=...)` (or the module-level `veil.get` etc.) |
| `@app.get("/x")`, `@app.post("/x")`, `@app.put`, `@app.delete`, `@app.patch` | the same decorators on `veil.App`, or `veil.get`, `veil.post`, ... |
| `@app.api_route("/x", methods=[...])` | `@app.route("/x", methods=[...])` |
| `uvicorn.run(app, host=..., port=...)` | `app.run(listen_cleartext="host:port")` (blocks) |
| `raise HTTPException(404, "not found")` | `raise veil.HTTPException(404, "not found")` (`{"detail": "not found"}`) |
| path and query parameters bound by name and annotation | the same (`int`/`float`/`bool`/`str`; failures give 422) |
| pydantic `BaseModel` body | the same when pydantic is installed (422 in FastAPI's error shape) |
| `Request` | `veil.Request` (`method`, `path`, `query`, `headers`, `body`, `json()`, `text()`, `params`, `query_params`, `client_ip`) |
| `Response`, `JSONResponse`, `StreamingResponse` | `veil.Response`, `veil.JSONResponse`, `veil.StreamingResponse`; also `veil.EventSourceResponse` |
| `status_code=201` in the decorator | `status_code=201` in the decorator |
| `Depends(...)` | not provided; see {doc}`dependency-injection` |
| `@app.websocket("/ws")` with `WebSocket` (`accept`, `receive_text`, `send_text`, `iter_text`, ...), `WebSocketDisconnect`, `WebSocketEndpoint` | the same names on `veil` (`veil.WebSocket`, ...); listed in OpenAPI too; see {doc}`websocket` |
| `async def` handlers on the event loop | the same (a persistent asyncio loop, uvloop when installed); `veil.run_in_threadpool`; see {doc}`async` |
| `FastAPI(lifespan=...)`, `@app.on_event("startup")` | `veil.App(lifespan=...)`, `@app.on_event(...)`, `@app.on_startup`, `app.state`; see {doc}`async` |
| OpenAPI at `/openapi.json`, docs at `/docs` | the same |

Differences worth knowing:

- Handlers hold the GIL while they run. By default (`offload=True`) they run on veil's handler pool so the
  event loop (the network workers) never waits for the GIL. `offload=False` runs the handler directly on the
  network worker and is only for trivially cheap handlers (a constant response, say). Pure-Python handlers are
  limited by the GIL's parallelism; for CPU-bound work consider several processes.
- `async def` handlers run on a persistent asyncio event loop (like uvicorn; do not block in them, use
  `veil.run_in_threadpool`). See {doc}`async`.
- Validation without pydantic is available through `body_constraints`.
