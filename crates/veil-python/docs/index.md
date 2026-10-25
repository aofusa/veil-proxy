# veil-api

`veil-api` is the Python binding of the [veil](https://github.com/aofusa/veil-proxy) API server library. HTTP
handling, TLS, HTTP/1.1, HTTP/2, HTTP/3 and OpenAPI serving run in Rust on veil's own io_uring data plane;
Python only supplies handler functions, written with FastAPI-like decorators.

```bash
pip install veil-api
pip install pydantic        # optional: body validation and schema generation
pip install "veil-api[grpc]"   # optional: grpcio-generated code for veil.grpc.Server
```

The import name is `veil`. The wheel is abi3 (Python 3.9 or later, one wheel for all versions).

```python
import veil

@veil.get("/hello/{name}")
def hello(name: str, greeting: str = "Hello"):
    return {"message": f"{greeting}, {name}"}

veil.run(listen_cleartext="127.0.0.1:8000")      # Ctrl-C / SIGTERM stops gracefully
```

```{toctree}
:maxdepth: 2
:caption: Guide

quickstart
routing
requests-responses
dependency-injection
async
websocket
grpc
configuration
```

The API reference (generated from the type stubs and docstrings) follows the guide.

## Links

- Source: <https://github.com/aofusa/veil-proxy>
- Changelog: <https://github.com/aofusa/veil-proxy/releases>
- Library guide (Rust, C and Python): `docs/guide/library.md` in the repository
