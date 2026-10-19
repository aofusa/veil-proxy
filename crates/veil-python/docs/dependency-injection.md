# Dependency injection (argument binding)

veil binds handler arguments with `inspect.signature`, by name and annotation. There is **no** `Depends(...)`
function: a handler receives only what the request supplies.

| Parameter | Bound to |
|-----------|----------|
| named `req` / `request`, or annotated `veil.Request` | the request object |
| same name as a path placeholder `{name}` | the path parameter (converted by an `int`/`float`/`bool`/`str` annotation; a failure gives 422) |
| annotated with a pydantic `BaseModel`, named `body`, or annotated `dict`/`list` | the JSON body (with pydantic, `model_validate_json`; a failure gives a FastAPI-shaped 422) |
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

## Sharing resources

Because there is no `Depends`, share resources the usual Python way: module-level objects, closures, or a
small factory function that you call yourself. Handlers run on several handler-pool threads (and `async def`
handlers on one event loop per thread), so a shared object must be thread-safe.

```python
import sqlite3, threading
_local = threading.local()

def db():
    if not hasattr(_local, "conn"):
        _local.conn = sqlite3.connect("app.db")
    return _local.conn

@veil.get("/count")
def count():
    return {"n": db().execute("select count(*) from t").fetchone()[0]}
```
