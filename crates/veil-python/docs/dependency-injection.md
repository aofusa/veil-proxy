# Dependency injection and parameters

veil binds handler arguments the way FastAPI does: by name and annotation, with `Depends`, `Security` and the
parameter markers `Query()`, `Path()`, `Header()`, `Cookie()`, `Body()`, `Form()` and `File()`. The binding of a
route (the dependency tree included) is built **once**, from the signatures, when the route is first used; a request
only calls the functions in the stored order. Routes that use none of this keep the plain binding below and pay
nothing for it.

## Plain binding

| Parameter | Bound to |
|-----------|----------|
| named `req` / `request`, or annotated `veil.Request` | the request object |
| same name as a path placeholder `{name}` | the path parameter (converted by an `int`/`float`/`bool`/`str` annotation; a failure gives 422) |
| annotated with a pydantic `BaseModel`, named `body`, or annotated `dict`/`list` | the JSON body (with pydantic, `model_validate_json`; a failure gives a FastAPI-shaped 422) |
| annotated `veil.Response` | a response whose headers, cookies and status are merged into the one returned |
| annotated `veil.BackgroundTasks` | the tasks run after the response (below) |
| anything else | a query parameter (with a default: optional; without: required; a missing or unconvertible value gives 422) |

```python
import veil
from pydantic import BaseModel

class User(BaseModel):
    name: str
    age: int

@veil.post("/users/{team}")
def add_user(team: str, user: User, notify: bool = False):
    return {"team": team, "user": user, "notify": notify}
```

## Parameter markers

The markers say where a value comes from and how it is checked. They work as defaults or as `Annotated` metadata:

```python
from typing import Annotated, Optional
from veil import Query, Path, Header, Cookie, Body

@veil.get("/items/{item_id}")
def item(
    item_id: int = Path(ge=1),
    q: Annotated[Optional[str], Query(max_length=20)] = None,
    tags: list[str] = Query([]),                      # ?tags=a&tags=b
    user_agent: Optional[str] = Header(None),         # the "user-agent" header (_ becomes -)
    session: Optional[str] = Cookie(None),
):
    return {"id": item_id, "q": q, "tags": tags, "ua": user_agent, "session": session}

@veil.post("/items")
def create(name: str = Body(embed=True)):             # {"name": "..."}
    return {"name": name}
```

`gt`, `ge`, `lt`, `le`, `min_length`, `max_length` and `pattern` are checked (with pydantic when it is installed,
without it by veil itself); a violation is a 422 in FastAPI's error shape. `alias`, `description`, `title`,
`examples`, `deprecated` and `include_in_schema` go to OpenAPI. `Form()` and `File()` are described in
{doc}`requests-responses`.

## `Depends`

```python
from veil import Depends, HTTPException

def pagination(skip: int = 0, limit: int = Query(10, le=100)):
    return {"skip": skip, "limit": limit}

def db():
    conn = connect()
    try:
        yield conn              # the code after yield runs when the handler has returned
    finally:
        conn.close()

def current_user(token: str = Header(), conn=Depends(db)):
    user = conn.user_for(token)
    if user is None:
        raise HTTPException(401, "invalid token")
    return user

@veil.get("/items")
def items(page: dict = Depends(pagination), user=Depends(current_user), conn=Depends(db)):
    return conn.items(user, **page)
```

- A dependency takes parameters like a handler (markers, the request, other `Depends`), at any depth.
- Plain functions, `async def` functions, generators and async generators work. The code after `yield` runs when the
  handler has returned, before the response is sent; an exception of the handler is raised at the `yield`.
- `use_cache=True` (the default): a dependency used several times in one request is called once (`db` above).
- `Depends()` with no argument calls the annotation: `form: OAuth2PasswordRequestForm = Depends()`.
- `dependencies=[Depends(check)]` on a route, an `APIRouter` or `include_router` runs dependencies whose value is
  not needed.
- On `async def` routes, plain (non-async) dependencies run on the thread pool, as in FastAPI. Cheap functions can be
  marked `fn._veil_inline = True` to be called directly.

### `Security` and `SecurityScopes`

`Security(dep, scopes=[...])` is a `Depends` that also requires OAuth2 scopes. A dependency below it declares a
`SecurityScopes` parameter to receive them; the security schemes of `veil_kit.security` are listed in OpenAPI
(`components.securitySchemes` and the operation's `security`). See {doc}`kit`.

### Overriding dependencies in tests

```python
app.dependency_overrides[current_user] = lambda: FakeUser()
...
app.dependency_overrides.clear()
```

A change of `app.dependency_overrides` rebuilds the trees; a request in flight keeps the old one.

## `BackgroundTasks`

```python
from veil import BackgroundTasks

def send_mail(to: str, body: str): ...

@veil.post("/signup")
def signup(email: str, tasks: BackgroundTasks):
    tasks.add_task(send_mail, email, "welcome")
    return {"ok": True}
```

The tasks run in order **after the response has been sent**: plain functions on the handler pool, `async def`
functions on the event loop. A failing task is logged and does not stop the next ones. `veil.Response(...,
background=tasks)` (and `StreamingResponse`, `JSONResponse`, ...) attaches tasks to a response you build yourself.

## `APIRouter`

```python
from veil import APIRouter

router = APIRouter(prefix="/items", tags=["items"], dependencies=[Depends(current_user)])

@router.get("/{item_id}")
def get_item(item_id: int):
    ...

app.include_router(router, prefix="/v1")    # GET /v1/items/{item_id}
```

## Sharing resources

For objects shared by the whole process, a module-level object, a dependency with a cache of your own or the
container of `veil_kit.di` ({doc}`kit`) work. Handlers run on several handler-pool threads (and `async def` handlers
on one event loop), so a shared object must be thread-safe.
