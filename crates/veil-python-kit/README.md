# veil-api-kit

Helpers for [veil-api](https://pypi.org/project/veil-api/) applications (`import veil_kit`). It is a pure-Python
package with the same version as `veil-api`. Optional dependencies come with extras:

```sh
pip install veil-api-kit                     # cookies, sessions, CORS, security, static files, DI, tasks
pip install "veil-api-kit[jwt]"              # JWT (PyJWT)
pip install "veil-api-kit[graphql]"          # GraphQL (strawberry)
pip install "veil-api-kit[grpc]"             # gRPC client and server helpers (protobuf)
pip install "veil-api-kit[grpcio]"           # + grpcio and grpcio-tools (code generation, grpc.RpcError)
pip install "veil-api-kit[redis]"            # Redis session store
pip install "veil-api-kit[cron]"             # croniter (more cron syntax)
pip install "veil-api-kit[all]"
```

| Module | What it does |
|---|---|
| `veil_kit.cookies` | Signed cookies (itsdangerous-compatible format, key rotation) |
| `veil_kit.sessions` | Cookie sessions (Starlette `SessionMiddleware` format) and server-side stores (memory, Redis) |
| `veil_kit.cors` | `CORSMiddleware` with Starlette's arguments, answered natively by the server |
| `veil_kit.security` | `HTTPBasic`, `HTTPBearer`, `APIKeyHeader`, `OAuth2PasswordBearer`, `OAuth2PasswordRequestForm`, `JWT` |
| `veil_kit.redirects` | `safe_redirect`: redirect only to this site or to listed hosts |
| `veil_kit.staticfiles` | `StaticFiles(directory=...)` served by the server's file backend (sendfile) |
| `veil_kit.graphql` | strawberry schemas over HTTP, SSE and WebSocket, with GraphiQL |
| `veil_kit.grpc` | A channel for `grpc_tools` stubs (sync and `grpc.aio` style), server helpers |
| `veil_kit.di` | A dependency injection container (singleton, scoped, transient) |
| `veil_kit.tasks` | Periodic, cron and resident tasks |
| `veil_kit.testing` | `TestClient` and dependency overrides |

## Example

```python
import veil
from veil_kit.cors import CORSMiddleware
from veil_kit.security import OAuth2PasswordBearer, OAuth2PasswordRequestForm, JWT
from veil_kit.staticfiles import StaticFiles

app = veil.App()
app.add_middleware(CORSMiddleware, allow_origins=["https://app.example.com"])
app.mount("/static", StaticFiles(directory="public"))

oauth2 = OAuth2PasswordBearer(tokenUrl="/token")
tokens = JWT(secret="change-me")

@app.post("/token")
def token(form: OAuth2PasswordRequestForm = veil.Depends()):
    if form.password != "secret":
        raise veil.HTTPException(400, "bad credentials")
    return {"access_token": tokens.encode({"sub": form.username}), "token_type": "bearer"}

def current_user(token: str = veil.Depends(oauth2)):
    return tokens.decode(token)["sub"]

@app.get("/me")
def me(user: str = veil.Depends(current_user)):
    return {"user": user}

app.run(listen_cleartext="127.0.0.1:8000")
```

Documentation: <https://veil-api.readthedocs.io> (the "Kit" and "FastAPI compatibility" pages).

License: Apache-2.0 OR MIT.
