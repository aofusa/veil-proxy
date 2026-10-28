"""veil-api-kit (``veil_kit``) and the FastAPI-style features of veil-api in one application (F-268).

    pip install veil-api "veil-api-kit[jwt,graphql]" fastapi     # fastapi only for the mounted ASGI app
    VEIL_LISTEN=127.0.0.1:8000 python app.py

- ``Depends`` / ``Query`` / ``Header`` / ``APIRouter`` / ``BackgroundTasks`` / ``response_model`` (native routes)
- OAuth2 password flow with JWT (``veil_kit.security``), sessions (``veil_kit.sessions``)
- CORS and static files answered by the server (``veil_kit.cors``, ``veil_kit.staticfiles``)
- a dependency injection container (``veil_kit.di``), a periodic task (``veil_kit.tasks``)
- GraphQL with strawberry (``veil_kit.graphql``), a FastAPI application mounted at ``/legacy`` (ASGI)
"""

import datetime
import os
import threading
from typing import List, Optional

import veil
from veil import APIRouter, BackgroundTasks, Depends, Header, HTTPException, Query, Security

from veil_kit.cors import CORSMiddleware
from veil_kit.di import Container, Inject
from veil_kit.redirects import safe_redirect
from veil_kit.security import JWT, OAuth2PasswordBearer, OAuth2PasswordRequestForm, requires_scopes
from veil_kit.sessions import Session, Sessions
from veil_kit.staticfiles import StaticFiles
from veil_kit.tasks import Tasks

HERE = os.path.dirname(os.path.abspath(__file__))
app = veil.App(title="veil kit example", version="1.0.0")

# ---- CORS・静的ファイル（サーバが答える） ----
app.add_middleware(CORSMiddleware, allow_origins=["https://app.example"], allow_methods=["GET", "POST"],
                   allow_headers=["authorization", "content-type"], allow_credentials=True)
app.mount("/static", StaticFiles(directory=os.path.join(HERE, "static"), html=True))

# ---- DI のコンテナ ----


class Settings:
    def __init__(self):
        self.greeting = os.environ.get("VEIL_GREETING", "hello")


class Greeter:
    def __init__(self, settings: Settings):
        self.settings = settings

    def greet(self, name):
        return "%s %s" % (self.settings.greeting, name)


container = Container()
container.singleton(Settings)
container.scoped(Greeter)

# ---- Depends・引数の印・APIRouter ----


def pagination(skip: int = Query(0, ge=0), limit: int = Query(10, le=100)):
    return {"skip": skip, "limit": limit}


def api_version(x_api_version: Optional[str] = Header(None)):
    return x_api_version or "1"


items = APIRouter(prefix="/items", tags=["items"])


@items.get("/{item_id}")
def get_item(item_id: int, page: dict = Depends(pagination), version: str = Depends(api_version),
             greeter: Greeter = Inject(Greeter)):
    return {"id": item_id, "page": page, "version": version, "greeting": greeter.greet("item")}


app.include_router(items, prefix="/v1")

# ---- BackgroundTasks（応答の後に走る） ----
_log = []
_log_lock = threading.Lock()


def write_log(msg):
    with _log_lock:
        _log.append(msg)


@app.post("/notify")
def notify(email: str = Query(), tasks: BackgroundTasks = None):
    tasks.add_task(write_log, "sent to " + email)
    return {"queued": True}


@app.get("/log")
def log():
    with _log_lock:
        return {"log": list(_log)}


# ---- 認証（OAuth2 のパスワードフロー + JWT）・スコープ ----
oauth2 = OAuth2PasswordBearer(tokenUrl="/token", scopes={"items:write": "write items"})
tokens = JWT(secret=os.environ.get("VEIL_JWT_SECRET", "change-me-to-a-long-random-secret-0123456789"))
USERS = {"alice": "wonderland"}


@app.post("/token")
def token(form: OAuth2PasswordRequestForm = Depends()):
    if USERS.get(form.username) != form.password:
        raise HTTPException(400, "Incorrect username or password")
    exp = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(minutes=10)
    claims = {"sub": form.username, "scopes": form.scopes, "exp": exp}
    return {"access_token": tokens.encode(claims), "token_type": "bearer"}


def current_user(token: str = Depends(oauth2)):
    return tokens.decode(token)["sub"]


write_scope = requires_scopes(oauth2, lambda t: tokens.decode(t).get("scopes", []))


@app.get("/me")
def me(user: str = Depends(current_user)):
    return {"user": user}


@app.post("/items", status_code=201)
def create_item(name: str = Query(), user: str = Depends(current_user),
                _t=Security(write_scope, scopes=["items:write"])):
    return {"name": name, "owner": user}


# ---- response_model（pydantic があれば絞り込む） ----
try:
    import pydantic

    class UserOut(pydantic.BaseModel):
        name: str

    @app.get("/users", response_model=List[UserOut])
    def users():
        return [{"name": "alice", "password": "secret"}]
except ImportError:  # pragma: no cover
    pass

# ---- セッション・リダイレクト ----
sessions = Sessions(secret_key="session-secret")


@app.get("/count")
def count(session: Session = Depends(sessions)):
    session["n"] = session.get("n", 0) + 1
    return {"n": session["n"]}


@app.get("/go")
def go(next: str = "/"):
    return safe_redirect(next, allowed_hosts=["app.example"])


@app.get("/internal", include_in_schema=False)
def internal():
    return "hidden"


# ---- 定期タスク ----
tasks = Tasks(app)
_ticks = {"n": 0}


@tasks.every(seconds=0.2, run_at_start=True)
async def tick():
    _ticks["n"] += 1


@app.get("/ticks")
def ticks():
    return _ticks


# ---- GraphQL（strawberry があれば） ----
try:
    import strawberry

    from veil_kit.graphql import mount as mount_graphql

    @strawberry.type
    class Query_:
        @strawberry.field
        def hello(self, name: str = "world") -> str:
            return "hello " + name

    @strawberry.type
    class Subscription:
        @strawberry.subscription
        async def count(self, to: int = 2) -> int:
            for i in range(1, to + 1):
                yield i

    mount_graphql(app, "/graphql", strawberry.Schema(query=Query_, subscription=Subscription))
except ImportError:  # pragma: no cover
    pass

# ---- FastAPI のアプリを /legacy に（ASGI。fastapi があれば） ----
try:
    import fastapi

    legacy = fastapi.FastAPI()

    @legacy.get("/hello/{name}")
    async def legacy_hello(name: str, q: int = 1):
        return {"hello": name, "q": q, "from": "fastapi"}

    @legacy.websocket("/ws")
    async def legacy_ws(websocket: fastapi.WebSocket):
        await websocket.accept()
        msg = await websocket.receive_text()
        await websocket.send_text("fastapi:" + msg)
        await websocket.close()

    app.mount("/legacy", legacy)
except ImportError:  # pragma: no cover
    pass

if __name__ == "__main__":
    app.run(listen_cleartext=os.environ.get("VEIL_LISTEN", "127.0.0.1:8000"))
