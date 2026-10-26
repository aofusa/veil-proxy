# F-268: FastAPI の書き方との互換（Depends・引数の印・APIRouter・BackgroundTasks・TestClient・ASGI）の単体試験。
# ネイティブ拡張を使わない（TestClient はサーバ無しでルートを呼ぶ）。
import asyncio
import os
import sys
import tempfile
import unittest
from typing import Annotated, List, Optional

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "python"))

import veil  # noqa: E402
from veil import (  # noqa: E402
    APIRouter,
    BackgroundTasks,
    Body,
    Cookie,
    Depends,
    Header,
    HTTPException,
    Path,
    Query,
    Response,
    Security,
    SecurityScopes,
)
from veil.testclient import TestClient  # noqa: E402

try:
    import pydantic
except ImportError:  # pragma: no cover
    pydantic = None


class TestDepends(unittest.TestCase):
    def test_nested_cache_and_params(self):
        app = veil.App()
        calls = []

        def common(q: Optional[str] = None, limit: int = Query(10, le=100)):
            calls.append("common")
            return {"q": q, "limit": limit}

        def user(c: dict = Depends(common), x_token: str = Header()):
            return {"c": c, "token": x_token}

        @app.get("/items/{item_id}")
        def items(item_id: int = Path(ge=1), c: dict = Depends(common), u: dict = Depends(user)):
            return {"id": item_id, "c": c, "u": u}

        cl = TestClient(app)
        r = cl.get("/items/3?q=a&limit=5", headers={"x-token": "t"})
        self.assertEqual(r.status_code, 200, r.text)
        self.assertEqual(r.json(), {"id": 3, "c": {"q": "a", "limit": 5}, "u": {"c": {"q": "a", "limit": 5}, "token": "t"}})
        self.assertEqual(calls, ["common"])  # 1 要求で 1 回（use_cache）
        r = cl.get("/items/0?limit=500")
        self.assertEqual(r.status_code, 422)
        locs = sorted(tuple(e["loc"]) for e in r.json()["detail"])
        self.assertEqual(locs, [("header", "x-token"), ("path", "item_id"), ("query", "limit")])

    def test_annotated_and_list_query(self):
        app = veil.App()

        @app.get("/s")
        def search(q: Annotated[List[str], Query()] = ["x"], n: Annotated[int, Query(gt=0)] = 1):
            return {"q": q, "n": n}

        cl = TestClient(app)
        self.assertEqual(cl.get("/s?q=a&q=b&n=2").json(), {"q": ["a", "b"], "n": 2})
        self.assertEqual(cl.get("/s").json(), {"q": ["x"], "n": 1})
        self.assertEqual(cl.get("/s?n=0").status_code, 422)

    def test_pattern_is_a_search(self):
        # B-171: pattern は JSON Schema・pydantic と同じく部分一致（固定は ^・$ を書く）
        app = veil.App()

        @app.get("/p")
        def p(q: str = Query(pattern="^a")):
            return q

        @app.post("/b", body_constraints={"name": {"pattern": "b"}})
        def b(body: dict):
            return body

        cl = TestClient(app)
        self.assertEqual(cl.get("/p?q=ab").status_code, 200)
        self.assertEqual(cl.get("/p?q=ba").status_code, 422)
        self.assertEqual(cl.post("/b", json={"name": "abc"}).status_code, 200)
        self.assertEqual(cl.post("/b", json={"name": "xyz"}).status_code, 422)

    def test_yield_teardown_and_exception(self):
        app = veil.App()
        log = []

        def db():
            log.append("open")
            try:
                yield "conn"
            except HTTPException:
                log.append("rollback")
                raise
            finally:
                log.append("close")

        @app.get("/ok")
        def ok(conn: str = Depends(db)):
            log.append("handler")
            return {"conn": conn}

        @app.get("/fail")
        def fail(conn: str = Depends(db)):
            raise HTTPException(409, "conflict")

        cl = TestClient(app)
        self.assertEqual(cl.get("/ok").json(), {"conn": "conn"})
        self.assertEqual(log, ["open", "handler", "close"])
        log.clear()
        r = cl.get("/fail")
        self.assertEqual(r.status_code, 409)
        self.assertEqual(log, ["open", "rollback", "close"])

    def test_async_route_with_sync_and_async_deps(self):
        app = veil.App()
        order = []

        async def agen():
            order.append("a-open")
            yield 1
            order.append("a-close")

        def sync_dep():
            return 2

        class Counter:
            def __init__(self, start: int = 0):
                self.n = start

        @app.get("/a")
        async def a(x: int = Depends(agen), y: int = Depends(sync_dep), c: Counter = Depends()):
            return {"x": x, "y": y, "c": c.n}

        cl = TestClient(app)
        self.assertEqual(cl.get("/a?start=7").json(), {"x": 1, "y": 2, "c": 7})
        self.assertEqual(order, ["a-open", "a-close"])

    def test_overrides_and_route_dependencies(self):
        app = veil.App()
        seen = []

        def real():
            return "real"

        def verify(x_key: str = Header()):
            if x_key != "k":
                raise HTTPException(403, "bad key")
            seen.append("verified")

        @app.get("/v", dependencies=[Depends(verify)])
        def v(s: str = Depends(real)):
            return s

        cl = TestClient(app)
        self.assertEqual(cl.get("/v", headers={"x-key": "k"}).text, "real")
        self.assertEqual(cl.get("/v", headers={"x-key": "no"}).status_code, 403)
        app.dependency_overrides[real] = lambda: "fake"
        self.assertEqual(cl.get("/v", headers={"x-key": "k"}).text, "fake")
        app.dependency_overrides.clear()
        self.assertEqual(cl.get("/v", headers={"x-key": "k"}).text, "real")
        self.assertEqual(seen, ["verified"] * 3)

    def test_security_scopes(self):
        app = veil.App()

        def token(scopes: SecurityScopes, authorization: str = Header()):
            return {"scopes": scopes.scopes, "auth": authorization}

        @app.get("/me")
        def me(t: dict = Security(token, scopes=["read"])):
            return t

        cl = TestClient(app)
        self.assertEqual(cl.get("/me", headers={"authorization": "x"}).json(), {"scopes": ["read"], "auth": "x"})


class TestParams(unittest.TestCase):
    def test_header_cookie_body(self):
        app = veil.App()

        @app.post("/p")
        def p(user_agent: str = Header(), sid: Optional[str] = Cookie(None), name: str = Body(embed=True),
              n: int = Body(embed=True, ge=0)):
            return {"ua": user_agent, "sid": sid, "name": name, "n": n}

        cl = TestClient(app)
        r = cl.post("/p", json={"name": "a", "n": 1}, cookies={"sid": "s1"})
        self.assertEqual(r.json(), {"ua": "testclient", "sid": "s1", "name": "a", "n": 1})
        r = cl.post("/p", json={"name": "a", "n": -1})
        self.assertEqual(r.status_code, 422)
        self.assertEqual(r.json()["detail"][0]["loc"], ["body", "n"])

    def test_response_param_and_cookies(self):
        app = veil.App()

        @app.post("/login", status_code=200)
        def login(response: Response):
            response.set_cookie("session", "abc", httponly=True)
            response.status_code = 201
            return {"ok": True}

        @app.get("/who")
        def who(request: veil.Request):
            return {"session": request.cookies.get("session")}

        cl = TestClient(app)
        r = cl.post("/login")
        self.assertEqual(r.status_code, 201)
        self.assertIn("HttpOnly", r.headers["set-cookie"])
        self.assertEqual(cl.get("/who").json(), {"session": "abc"})  # クライアントの Cookie の保持


class TestBackground(unittest.TestCase):
    def test_runs_after_response(self):
        app = veil.App()
        log = []

        async def notify(x):
            log.append(("async", x))

        @app.post("/send")
        def send(tasks: BackgroundTasks):
            tasks.add_task(log.append, ("sync", 1))
            tasks.add_task(notify, 2)
            log.append("handler")
            return {"queued": len(tasks.tasks)}

        r = TestClient(app).post("/send")
        self.assertEqual(r.json(), {"queued": 2})
        self.assertEqual(log, ["handler", ("sync", 1), ("async", 2)])

    def test_response_background_attribute(self):
        app = veil.App()
        log = []

        @app.get("/r")
        def r():
            return veil.JSONResponse({"a": 1}, background=lambda: log.append("bg"))

        TestClient(app).get("/r")
        self.assertEqual(log, ["bg"])


class TestRouter(unittest.TestCase):
    def test_prefix_tags_dependencies(self):
        app = veil.App()
        router = APIRouter(prefix="/users", tags=["users"])
        hits = []

        def audit():
            hits.append(1)

        @router.get("/{uid}")
        def get_user(uid: int):
            return {"uid": uid}

        @router.post("/", status_code=201)
        def create():
            return {"created": True}

        app.include_router(router, prefix="/v1", dependencies=[Depends(audit)])
        cl = TestClient(app)
        self.assertEqual(cl.get("/v1/users/5").json(), {"uid": 5})
        self.assertEqual(cl.post("/v1/users/").status_code, 201)
        self.assertEqual(cl.get("/v1/users/x").status_code, 422)
        self.assertEqual(cl.delete("/v1/users/5").status_code, 405)
        self.assertEqual(cl.get("/nope").status_code, 404)
        self.assertEqual(len(hits), 3)
        r = app.routes[0]
        self.assertEqual(r.tags, ["users"])
        self.assertEqual(r.path, "/v1/users/{uid}")


@unittest.skipIf(pydantic is None, "pydantic is not installed")
class TestModels(unittest.TestCase):
    def test_response_model_filters(self):
        class UserIn(pydantic.BaseModel):
            name: str
            password: str

        class UserOut(pydantic.BaseModel):
            name: str

        app = veil.App()

        @app.post("/u", response_model=UserOut)
        def create(u: UserIn):
            return u

        @app.get("/us", response_model=List[UserOut])
        def many():
            return [{"name": "a", "password": "x"}, {"name": "b", "password": "y"}]

        cl = TestClient(app)
        self.assertEqual(cl.post("/u", json={"name": "a", "password": "p"}).json(), {"name": "a"})
        self.assertEqual(cl.get("/us").json(), [{"name": "a"}, {"name": "b"}])
        op = app.routes[1].openapi()
        self.assertEqual(op["responses"]["200"]["content"]["application/json"]["schema"]["type"], "array")


class TestOpenAPI(unittest.TestCase):
    def test_dependency_params_and_security(self):
        class Bearer:
            _veil_inline = True

            def openapi_scheme(self):
                return "BearerAuth", {"type": "http", "scheme": "bearer"}

            def __call__(self, authorization: str = Header()):
                return authorization

        bearer = Bearer()
        app = veil.App()

        def page(skip: int = 0, limit: int = Query(10, le=50, description="page size")):
            return skip, limit

        @app.get("/items", responses={404: {"description": "Missing"}}, deprecated=True)
        def items(p=Depends(page), tok: str = Security(bearer, scopes=["items:read"])):
            return {}

        op = app.routes[0].openapi()
        names = {(x["in"], x["name"]) for x in op["parameters"]}
        self.assertEqual(names, {("query", "skip"), ("query", "limit"), ("header", "authorization")})
        limit = next(x for x in op["parameters"] if x["name"] == "limit")
        self.assertEqual(limit["schema"]["maximum"], 50)
        self.assertEqual(limit["description"], "page size")
        self.assertEqual(op["security"], [{"BearerAuth": ["items:read"]}])
        self.assertEqual(op["x-veil-security-schemes"]["BearerAuth"]["scheme"], "bearer")
        self.assertEqual(op["responses"]["404"]["description"], "Missing")
        self.assertTrue(op["deprecated"])

    def test_include_in_schema(self):
        app = veil.App()

        @app.get("/hidden", include_in_schema=False)
        def hidden():
            return {}

        self.assertFalse(app.routes[0].include_in_schema)


class TestResponses(unittest.TestCase):
    def test_classes(self):
        app = veil.App()
        d = tempfile.mkdtemp()
        fp = os.path.join(d, "a.txt")
        with open(fp, "w") as f:
            f.write("file body")

        @app.get("/html", response_class=veil.HTMLResponse)
        def html():
            return "<h1>x</h1>"

        @app.get("/go")
        def go():
            return veil.RedirectResponse("/html", status_code=302)

        @app.get("/f")
        def f():
            return veil.FileResponse(fp, filename="a.txt")

        cl = TestClient(app, follow_redirects=False)
        r = cl.get("/html")
        self.assertEqual(r.headers["content-type"], "text/html; charset=utf-8")
        self.assertEqual(cl.get("/go").headers["location"], "/html")
        self.assertEqual(TestClient(app).get("/go").text, "<h1>x</h1>")
        r = cl.get("/f")
        self.assertEqual(r.text, "file body")
        self.assertIn('filename="a.txt"', r.headers["content-disposition"])
        self.assertEqual(r.headers["content-type"], "text/plain")


class TestASGI(unittest.TestCase):
    def test_raw_asgi_app_and_lifespan(self):
        events = []

        async def asgi(scope, receive, send):
            if scope["type"] == "lifespan":
                while True:
                    m = await receive()
                    if m["type"] == "lifespan.startup":
                        events.append("startup")
                        await send({"type": "lifespan.startup.complete"})
                    elif m["type"] == "lifespan.shutdown":
                        events.append("shutdown")
                        await send({"type": "lifespan.shutdown.complete"})
                        return
            body = (await receive())["body"]
            await send({"type": "http.response.start", "status": 201,
                        "headers": [(b"content-type", b"text/plain"), (b"x-root", scope["root_path"].encode())]})
            await send({"type": "http.response.body", "body": b"echo:", "more_body": True})
            await send({"type": "http.response.body", "body": scope["path"].encode() + b":" + body})

        app = veil.App()
        app.mount("/legacy", asgi)

        @app.get("/native")
        def native():
            return "native"

        with TestClient(app) as cl:
            r = cl.post("/legacy/x", content=b"hi")
            self.assertEqual(r.status_code, 201)
            self.assertEqual(r.text, "echo:/legacy/x:hi")
            self.assertEqual(r.headers["x-root"], "/legacy")
            self.assertEqual(cl.get("/native").text, "native")
        self.assertEqual(events, ["startup", "shutdown"])

    def test_fastapi_app(self):
        try:
            import fastapi
        except ImportError:
            self.skipTest("fastapi is not installed")
        api = fastapi.FastAPI()

        @api.get("/hello/{name}")
        async def hello(name: str, q: int = 1):
            return {"hello": name, "q": q}

        app = veil.App()
        app.mount("/", api)
        with TestClient(app) as cl:
            self.assertEqual(cl.get("/hello/veil?q=3").json(), {"hello": "veil", "q": 3})
            self.assertEqual(cl.get("/hello/veil?q=x").status_code, 422)
            self.assertEqual(cl.get("/openapi.json").json()["paths"]["/hello/{name}"]["get"]["operationId"],
                             "hello_hello__name__get")


if __name__ == "__main__":
    asyncio.set_event_loop_policy(None)
    unittest.main(verbosity=2)
