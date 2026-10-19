# Routing and decorators

## Registering routes

Use the module-level decorators, or the same methods on an application object:

```python
import veil

@veil.get("/items/{item_id}")
def get_item(item_id: int):
    return {"id": item_id}

app = veil.App(title="Shop", version="1.0.0")

@app.post("/orders", status_code=201, tags=["orders"], summary="Create an order")
def create_order(body: dict):
    return body

app.run(listen_cleartext="0.0.0.0:8080")
```

The decorators are `get`, `post`, `put`, `delete`, `patch` and the general `route(path, methods=[...])`
(see {py:meth}`veil.App.route`). Common keyword arguments:

| Argument | Meaning |
|----------|---------|
| `offload` | `True` (default): run on the handler pool. `False`: run on the network worker (trivially cheap handlers only). |
| `status_code` | Default status of a normal return (replaces 200). |
| `body_constraints` | Validation of a dict body without pydantic (see below). |
| `tags`, `summary` | Put in the OpenAPI Operation Object. |
| `stream_body` | Call the handler before the request body has arrived; read it with `request.stream()`. |

The path uses `{name}` placeholders. A function parameter with the same name receives the (converted) value;
see {doc}`dependency-injection` for the full binding rules.

## Return values

| Return value | Response |
|--------------|----------|
| `dict`, `list`, numbers | JSON |
| `str` | `text/plain` |
| `bytes` | `application/octet-stream` |
| `veil.Response`, `veil.JSONResponse` | as given |
| `(body, status)` | `body` with that status |
| pydantic model | JSON |
| `None` | 204 |
| generator / async generator | streamed chunk by chunk (see {doc}`requests-responses`) |

`raise veil.HTTPException(404, "not found")` gives `{"detail": "not found"}` with status 404. An unhandled
exception in a handler prints the traceback to standard error and returns 500
`{"detail":"Internal Server Error"}`.

## `body_constraints` (validation without pydantic)

`minimum`, `maximum`, `exclusiveMinimum`, `exclusiveMaximum`, `minLength`, `maxLength`, `pattern`
(`re.fullmatch`), `enum` and `type` are checked for each field of a dict body (violations give 422).
Required fields are given as `{"required": ["age"]}` at the top level or `"required": True` inside a field.
The same constraints are reflected in the OpenAPI `requestBody`.

## OpenAPI

For every route veil builds the Operation Object in Python (operationId = function name, summary and
description from the docstring, tags, path and query parameters, `requestBody`, the 422 response) and serves
it at `/openapi.json` and `/docs`. pydantic model schemas go to `components.schemas`.
