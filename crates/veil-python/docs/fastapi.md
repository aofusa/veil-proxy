# FastAPI compatibility

There are two ways to bring a FastAPI application to veil:

1. **Native routes**: write the routes with `veil` (the same decorators, `Depends`, markers, `APIRouter`,
   `BackgroundTasks`, `response_model`, ...). Usually the change is the imports. This is the fast path: argument
   binding is decided when the route is registered and HTTP runs in Rust.
2. **ASGI**: run the FastAPI (or Starlette, or any ASGI 3) application as it is with `veil.asgi.run(api)` instead of
   uvicorn, or mount it under a path of a veil application with `app.mount("/legacy", api)`. Everything of FastAPI
   works (its middlewares, plugins, its own OpenAPI), and it is as fast as the ASGI application is.

A common migration is to mount the existing application, then move the busy routes to native ones.

## Native routes

```python
# from fastapi import FastAPI, APIRouter, Depends, Query, Header, HTTPException, BackgroundTasks
import veil
from veil import APIRouter, Depends, Query, Header, HTTPException, BackgroundTasks
from veil_kit.security import OAuth2PasswordBearer   # fastapi.security
from veil_kit.cors import CORSMiddleware             # fastapi.middleware.cors
from veil_kit.staticfiles import StaticFiles         # fastapi.staticfiles

app = veil.App(title="My API")                       # FastAPI(title="My API")
```

| FastAPI | veil |
|---------|------|
| `FastAPI(title=..., version=..., lifespan=...)` | `veil.App(title=..., version=..., lifespan=...)` |
| `@app.get` / `post` / `put` / `delete` / `patch` / `head` / `options` | the same |
| `@app.api_route(path, methods=[...])` | `@app.route(path, methods=[...])` |
| `Query()`, `Path()`, `Header()`, `Cookie()`, `Body()`, `Form()`, `File()`, `UploadFile`, `Annotated[...]` | the same (see {doc}`dependency-injection` and {doc}`requests-responses`) |
| `Depends`, `Security`, `SecurityScopes`, `yield` dependencies, `dependencies=[...]` | the same |
| `app.dependency_overrides` | the same |
| `BackgroundTasks` | the same (run after the response is sent) |
| `APIRouter(prefix=, tags=, dependencies=)`, `app.include_router(router, prefix=)` | the same |
| `response_model=`, `response_model_exclude_none=`, `status_code=`, `responses=`, `response_class=`, `include_in_schema=`, `deprecated=`, `operation_id=` | the same |
| `Response`, `JSONResponse`, `HTMLResponse`, `PlainTextResponse`, `RedirectResponse`, `StreamingResponse`, `FileResponse` | the same names on `veil` |
| `response.set_cookie()`, `response.delete_cookie()`, `request.cookies` | the same |
| `HTTPException(status, detail, headers)` | the same |
| `@app.websocket`, `WebSocket`, `WebSocketDisconnect` | the same (see {doc}`websocket`) |
| `app.add_middleware(CORSMiddleware, ...)` | the same with `veil_kit.cors.CORSMiddleware` (answered by the server) |
| `app.mount("/static", StaticFiles(directory=...))` | the same with `veil_kit.staticfiles.StaticFiles` (served by the server, sendfile) |
| `fastapi.security.*` (`OAuth2PasswordBearer`, `HTTPBasic`, `HTTPBearer`, `APIKeyHeader`, ...) | `veil_kit.security.*` |
| `fastapi.testclient.TestClient` | `veil.testclient.TestClient` (in-process) |
| `uvicorn.run(app, host=, port=)` | `app.run(listen_cleartext="host:port")` |
| `/openapi.json`, `/docs` | the same |

Not available on native routes (use an ASGI mount for these):

- Starlette middlewares (`@app.middleware("http")`, `BaseHTTPMiddleware`, `SessionMiddleware`, `GZipMiddleware`).
  CORS, compression (`[route.compression]`) and sessions (`veil_kit.sessions`) have native equivalents.
- `@app.exception_handler` (raise `HTTPException`, or return a response) and `request.form()` (declare `Form()` /
  `File()` parameters instead).
- `allow_origin_regex` of `CORSMiddleware` (list the origins; `https://*.example.com` patterns work).

## ASGI applications

```python
from fastapi import FastAPI
import veil, veil.asgi

api = FastAPI()

@api.get("/hello/{name}")
async def hello(name: str):
    return {"hello": name}

veil.asgi.run(api, listen_cleartext="0.0.0.0:8000")    # instead of uvicorn.run(api, ...)

# or next to native routes:
app = veil.App()
app.mount("/legacy", api)                              # /legacy/... goes to FastAPI
app.run(listen_cleartext="0.0.0.0:8000")
```

- Scopes: `http` (with the `http.response.trailers` and `http.response.pathsend` extensions), `websocket`
  (HTTP/1.1 upgrade) and `lifespan` (startup and shutdown run with the server).
- veil parses HTTP/1.1, HTTP/2 (h2c and TLS) and HTTP/3, terminates TLS and compresses; the application only
  sees `receive` / `send` on veil's persistent event loop. `root_path` is the mount path.
- The request body is read before the application is called (`[api] max_body_size` applies). The client port is not
  known (`client` is `(ip, 0)`). `http.response.pathsend` reads the file in Python.
  used only from trusted proxies, as with uvicorn's `--forwarded-allow-ips`: `veil.asgi.run(api,
  forwarded_allow_ips=["10.0.0.0/8"])` or `app.mount("/legacy", api, forwarded_allow_ips="10.0.0.1")` (`"*"` trusts
  every client); the default is the `FORWARDED_ALLOW_IPS` environment variable, or `127.0.0.1`.
- Background work after the response (Starlette `BackgroundTask`, FastAPI `BackgroundTasks`, also behind
  `@app.middleware("http")`) runs to completion after the last chunk was sent; the application is cancelled only
  when the client disconnects in the middle of a streamed response.
- The OpenAPI of a mounted application is its own (`/legacy/openapi.json`); veil's `/openapi.json` lists the native
  routes.
- An ASGI application runs on the same event loop as the native `async def` routes and holds the GIL while it runs, so
  a busy mounted application slows the native routes of the same process. Move the busy routes to native ones.

WSGI applications (Flask, Django) are not supported directly; an ASGI adapter such as `a2wsgi` works through
`veil.asgi`.
