# The kit (`veil-api-kit`)

`veil-api-kit` is a pure-Python package of helpers for veil applications: cookies, sessions, CORS, authentication,
redirects, static files, GraphQL, a gRPC client, a dependency injection container and tasks. It has the same version
as `veil-api` and depends on exactly that version. The import name is `veil_kit`.

```bash
pip install veil-api-kit                  # no third-party dependency
pip install "veil-api-kit[jwt]"           # + PyJWT
pip install "veil-api-kit[graphql]"       # + strawberry-graphql
pip install "veil-api-kit[grpc]"          # + protobuf (gRPC client without grpcio)
pip install "veil-api-kit[grpcio]"        # + grpcio, grpcio-tools (code generation, grpc.RpcError)
pip install "veil-api-kit[redis]"         # + redis (server-side sessions)
pip install "veil-api-kit[cron]"          # + croniter (more cron syntax; a built-in parser is used without it)
pip install "veil-api-kit[all]"
```

The complete example is `examples/api/python-kit/app.py` in the repository.

## Cookies and sessions (`veil_kit.cookies`, `veil_kit.sessions`)

`request.cookies`, `response.set_cookie()` and `response.delete_cookie()` are part of `veil` (see
{doc}`requests-responses`). The kit adds signing and sessions:

```python
from veil_kit.cookies import Signer, SignedCookies
from veil_kit.sessions import Sessions, Session, MemoryStore

signer = Signer(["old-secret", "new-secret"])   # oldest first: signs with the last key, accepts all (key rotation)

sessions = Sessions(secret_key="change-me")     # the cookie holds the data (Starlette's SessionMiddleware format)
# Sessions(secret_key=..., store=MemoryStore()) keeps the data on the server; RedisStore(redis.Redis()) too

@app.get("/count")
def count(session: Session = Depends(sessions)):
    session["n"] = session.get("n", 0) + 1       # the cookie is written only when the session changed
    return {"n": session["n"]}
```

The signature format is that of `itsdangerous` (`TimestampSigner`), so cookies of a Flask or Starlette application
can be read. `Sessions` takes the arguments of Starlette's `SessionMiddleware` (`session_cookie`, `max_age`, `path`,
`same_site`, `https_only`, `domain`).

## CORS (`veil_kit.cors`)

```python
from veil_kit.cors import CORSMiddleware

app.add_middleware(CORSMiddleware, allow_origins=["https://app.example.com", "https://*.example.org"],
                   allow_methods=["GET", "POST"], allow_headers=["authorization"], allow_credentials=True)
```

The arguments are Starlette's. The server answers preflight requests and adds the headers (the same Rust
implementation as the Rust kit); Python is not called for them. `allow_origin_regex` is not supported.

## Authentication (`veil_kit.security`)

The schemes have FastAPI's names and are used with `Depends`; they appear in OpenAPI
(`components.securitySchemes`).

| Scheme | Gives |
|---|---|
| `HTTPBasic()` | `HTTPBasicCredentials` (`username`, `password`) |
| `HTTPBearer()`, `HTTPDigest()` | `HTTPAuthorizationCredentials` (`scheme`, `credentials`) |
| `APIKeyHeader(name=)`, `APIKeyCookie(name=)`, `APIKeyQuery(name=)` | the key |
| `OAuth2PasswordBearer(tokenUrl=, scopes=)`, `OAuth2AuthorizationCodeBearer(...)` | the bearer token |
| `OAuth2PasswordRequestForm` (`Depends()`), `OAuth2PasswordRequestFormStrict` | the login form |
| `JWT(secret=)` / `JWT(key, algorithms=["RS256"])` / `JWT(jwks_url=)` | `encode(claims)`, `decode(token)` (PyJWT; 401 when invalid) |
| `requires_scopes(scheme, verify)` | a dependency for `Security(..., scopes=[...])` that answers 401 when a scope is missing |
| `compare_digest(a, b)` | constant-time comparison for passwords and keys |

```python
from veil import Depends, Security, HTTPException
from veil_kit.security import OAuth2PasswordBearer, OAuth2PasswordRequestForm, JWT, requires_scopes

oauth2 = OAuth2PasswordBearer(tokenUrl="/token", scopes={"items:write": "write items"})
tokens = JWT(secret="a-long-random-secret")

@app.post("/token")
def token(form: OAuth2PasswordRequestForm = Depends()):
    if not check_password(form.username, form.password):
        raise HTTPException(400, "Incorrect username or password")
    return {"access_token": tokens.encode({"sub": form.username, "scopes": form.scopes}), "token_type": "bearer"}

def current_user(token: str = Depends(oauth2)):
    return tokens.decode(token)["sub"]

write = requires_scopes(oauth2, lambda t: tokens.decode(t).get("scopes", []))

@app.post("/items")
def create(user: str = Depends(current_user), _=Security(write, scopes=["items:write"])):
    ...
```

## Redirects (`veil_kit.redirects`)

`safe_redirect(url, allowed_hosts=["app.example.com"])` returns a `RedirectResponse` to `url` when it is a path of
this site or a URL of a listed host, and to `fallback` (`"/"`) otherwise (open redirects through a `?next=`
parameter). `is_safe_url(url, allowed_hosts)` is the check alone.

## Static files (`veil_kit.staticfiles`)

```python
from veil_kit.staticfiles import StaticFiles

app.mount("/static", StaticFiles(directory="public", html=True))
```

The directory is served by the server's file backend: `sendfile`, kTLS, Range, `ETag`, precompressed files and the
file caches work as for the proxy, and requests do not reach Python. `html=True` serves `index.html` for directories.
`app.serve_dir(path, directory, index=None)` is the same without the kit.

## GraphQL (`veil_kit.graphql`)

```python
import strawberry
from veil_kit.graphql import mount

@strawberry.type
class Query:
    @strawberry.field
    def hello(self, name: str = "world") -> str:
        return "hello " + name

mount(app, "/graphql", strawberry.Schema(query=Query))
```

`GET` and `POST` queries (batches too), GraphiQL for a browser `GET` (its scripts load from a CDN; `graphiql=False`
turns it off), and subscriptions over SSE (`Accept: text/event-stream`) and WebSocket (`graphql-transport-ws` and
`graphql-ws`, on the same path) are provided. Resolvers run on the event
loop. `context_getter=` builds the context from the request.

## gRPC (`veil_kit.grpc`)

The stubs generated by `grpc_tools.protoc` work with a channel of veil's own HTTP/2 client:

```python
import demo_pb2, demo_pb2_grpc                  # python -m grpc_tools.protoc --python_out=. --grpc_python_out=. demo.proto
from veil_kit import grpc as vgrpc

with vgrpc.insecure_channel("127.0.0.1:50051") as ch:
    stub = demo_pb2_grpc.DemoStub(ch)
    print(stub.SayHello(demo_pb2.HelloRequest(name="veil"), timeout=5).message)
    for r in stub.CountUp(demo_pb2.CountRequest(count=3)):
        print(r.n)

creds = vgrpc.ssl_channel_credentials(root_certificates=open("ca.pem", "rb").read())
ch = vgrpc.secure_channel("api.example.com:443", creds)

async def main():                               # grpc.aio style
    async with vgrpc.aio.insecure_channel("127.0.0.1:50051") as ch:
        r = await demo_pb2_grpc.DemoStub(ch).SayHello(demo_pb2.HelloRequest(name="aio"))
```

- All four kinds of calls, metadata, deadlines (`timeout=`), gzip (`compression=`), `with_call`, `future`. Errors raise
  `vgrpc.RpcError` (a `grpc.RpcError` when grpcio is installed) with `code()`, `details()`, `trailing_metadata()`.
- grpcio is not needed (only protobuf for the messages). The calls run on a client thread of veil; a blocking call
  releases the GIL while it waits, and the asyncio channel does not block the event loop.
- TLS verifies with the system trust store and `[tls_client] ca` (or `root_certificates`); `private_key` and
  `certificate_chain` give a client certificate.
- Servers: `veil_kit.grpc.Server` is `veil.grpc.Server`, which takes the generated `add_XxxServicer_to_server`
  functions (see {doc}`grpc`).

## Dependency injection container (`veil_kit.di`)

```python
from veil_kit.di import Container, Inject

container = Container()
container.singleton(Settings)                    # once per process
container.scoped(Session, make_session)          # once per request; make_session may yield (cleanup after)
container.transient(Clock)                       # on every use

@container.provider(Repo, scope="scoped")
def repo(session: Session, settings: Settings):  # parameters of registered types are injected
    return Repo(session, settings)

@app.get("/items")
def items(repo: Repo = Inject(Repo)):
    return repo.list()

with container.override(Repo, FakeRepo()):       # in tests
    ...
```

Providers are resolved through `Depends`, so they may also take request parameters; the resolution order is fixed
when the route is built. Singletons are created once, thread-safely.

## Tasks (`veil_kit.tasks`)

```python
from veil_kit.tasks import Tasks

tasks = Tasks(app)

@tasks.every(minutes=5)
async def refresh():
    ...

@tasks.cron("0 3 * * *")                        # minute hour day month weekday (UTC)
def nightly():
    ...

@tasks.background
async def consumer():                            # runs from startup to shutdown
    ...
```

Tasks start after the application's startup and are cancelled at shutdown. Plain functions run on the thread pool;
runs of one task never overlap, and an exception is logged without stopping the schedule.

## Testing (`veil_kit.testing`)

`TestClient` (in-process; see `veil.testclient`) and `override_dependency(app, dep, replacement)`, a context manager
around `app.dependency_overrides`.
