# F-268: veil-api-kit（veil_kit）の単体試験。サーバ無し（veil.testclient.TestClient）で動かす。
# gRPC のチャネルと CORS・静的ファイルの実際の応答はサーバが要るので tests/api/library_e2e.sh の python-kit で確かめる。
import base64
import datetime
import json
import os
import sys
import time
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, os.path.join(HERE, "..", "src"))
sys.path.insert(0, os.path.join(HERE, "..", "..", "veil-python", "python"))

import veil  # noqa: E402
from veil import Depends, Security  # noqa: E402
from veil.testclient import TestClient  # noqa: E402

from veil_kit import cookies, di, redirects, security, sessions, tasks, testing  # noqa: E402
from veil_kit.cors import CORSMiddleware  # noqa: E402
from veil_kit.staticfiles import StaticFiles  # noqa: E402

try:
    import jwt  # noqa: F401
except ImportError:  # pragma: no cover
    jwt = None
try:
    import strawberry
except ImportError:  # pragma: no cover
    strawberry = None
try:
    import itsdangerous
except ImportError:
    itsdangerous = None


class TestCookies(unittest.TestCase):
    def test_sign_unsign_rotation_expiry(self):
        s = cookies.Signer("k1")
        token = s.sign("hello")
        self.assertEqual(s.unsign(token), b"hello")
        with self.assertRaises(cookies.BadSignature):
            s.unsign(token[:-1] + (b"A" if token[-1:] != b"A" else b"B"))
        rotated = cookies.Signer(["k1", "k2"])
        self.assertEqual(rotated.unsign(token), b"hello")  # 古い鍵でも検証できる
        self.assertNotEqual(rotated.sign("hello").rsplit(b".", 1)[1], token.rsplit(b".", 1)[1])
        with self.assertRaises(cookies.SignatureExpired):
            s.unsign(token, max_age=-1)

    @unittest.skipIf(itsdangerous is None, "itsdangerous is not installed")
    def test_itsdangerous_compatible(self):
        ours = cookies.Signer("secret").sign("v")
        self.assertEqual(itsdangerous.TimestampSigner("secret").unsign(ours), b"v")
        theirs = itsdangerous.TimestampSigner("secret").sign("w")
        self.assertEqual(cookies.Signer("secret").unsign(theirs), b"w")

    def test_signed_cookies_roundtrip(self):
        app = veil.App()
        signed = cookies.SignedCookies("k", max_age=60)

        @app.post("/login")
        def login(response: veil.Response):
            signed.set(response, "user", "alice", httponly=True)
            return "ok"

        @app.get("/me")
        def me(request: veil.Request):
            return {"user": signed.get(request, "user")}

        cl = TestClient(app)
        cl.post("/login")
        self.assertEqual(cl.get("/me").json(), {"user": "alice"})
        cl.cookies["user"] = "forged"
        self.assertEqual(cl.get("/me").json(), {"user": None})


class TestSessions(unittest.TestCase):
    def _app(self, **kw):
        app = veil.App()
        s = sessions.Sessions("secret", **kw)

        @app.get("/count")
        def count(session: sessions.Session = Depends(s)):
            session["n"] = session.get("n", 0) + 1
            return {"n": session["n"]}

        @app.get("/peek")
        def peek(session: sessions.Session = Depends(s)):
            return {"n": session.get("n")}

        @app.post("/logout")
        def logout(session: sessions.Session = Depends(s)):
            session.clear()
            return "bye"

        return app

    def test_cookie_session(self):
        cl = TestClient(self._app())
        self.assertEqual(cl.get("/count").json(), {"n": 1})
        r = cl.get("/count")
        self.assertEqual(r.json(), {"n": 2})
        self.assertEqual(cl.get("/peek").json(), {"n": 2})
        # Starlette と同じ形式（base64 の JSON に署名）
        raw = cl.cookies["session"]
        payload = raw.rsplit(".", 2)[0]
        self.assertEqual(json.loads(base64.b64decode(payload)), {"n": 2})
        self.assertNotIn("set-cookie", cl.get("/peek").headers)  # 変えなければ書かない
        cl.post("/logout")
        self.assertEqual(cl.get("/peek").json(), {"n": None})

    def test_memory_store(self):
        store = sessions.MemoryStore()
        cl = TestClient(self._app(store=store))
        cl.get("/count")
        self.assertEqual(cl.get("/count").json(), {"n": 2})
        # Cookie には id だけ（データはサーバ側）
        self.assertFalse(cl.cookies["session"].startswith("eyJ"))
        self.assertEqual([v[0] for v in store._data.values()], [{"n": 2}])


class TestSecurity(unittest.TestCase):
    def test_basic_bearer_apikey(self):
        app = veil.App()
        basic = security.HTTPBasic(realm="r")
        bearer = security.HTTPBearer()
        key = security.APIKeyHeader(name="x-api-key", auto_error=False)

        @app.get("/basic")
        def b(c: security.HTTPBasicCredentials = Depends(basic)):
            return {"u": c.username, "p": c.password}

        @app.get("/bearer")
        def br(c: security.HTTPAuthorizationCredentials = Depends(bearer)):
            return {"t": c.credentials}

        @app.get("/key")
        def k(v=Depends(key)):
            return {"k": v}

        cl = TestClient(app)
        auth = "Basic " + base64.b64encode(b"al:pw").decode()
        self.assertEqual(cl.get("/basic", headers={"authorization": auth}).json(), {"u": "al", "p": "pw"})
        r = cl.get("/basic")
        self.assertEqual(r.status_code, 401)
        self.assertEqual(r.headers["www-authenticate"], 'Basic realm="r"')
        self.assertEqual(cl.get("/bearer", headers={"authorization": "Bearer t1"}).json(), {"t": "t1"})
        self.assertEqual(cl.get("/bearer").status_code, 401)
        self.assertEqual(cl.get("/key").json(), {"k": None})
        self.assertEqual(cl.get("/key", headers={"x-api-key": "s"}).json(), {"k": "s"})
        op = app.routes[0].openapi()
        self.assertEqual(op["x-veil-security-schemes"]["HTTPBasic"], {"type": "http", "scheme": "basic"})

    @unittest.skipIf(jwt is None, "PyJWT is not installed")
    def test_oauth2_password_jwt_scopes(self):
        app = veil.App()
        oauth2 = security.OAuth2PasswordBearer(tokenUrl="/token", scopes={"items": "read items"})
        tokens = security.JWT(secret="s3cret")

        @app.post("/token")
        def token(form: security.OAuth2PasswordRequestForm = Depends()):
            if form.password != "pw":
                raise veil.HTTPException(400, "bad credentials")
            exp = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(minutes=5)
            return {"access_token": tokens.encode({"sub": form.username, "scopes": form.scopes, "exp": exp}),
                    "token_type": "bearer"}

        def claims(t: str = Depends(oauth2)):
            return tokens.decode(t)

        check = security.requires_scopes(oauth2, lambda t: tokens.decode(t).get("scopes", []))

        @app.get("/items")
        def items(t=Security(check, scopes=["items"]), c: dict = Depends(claims)):
            return {"user": c["sub"]}

        cl = TestClient(app)
        self.assertEqual(cl.post("/token", data={"username": "u", "password": "no"}).status_code, 400)
        tok = cl.post("/token", data={"username": "u", "password": "pw", "scope": "items"}).json()["access_token"]
        self.assertEqual(cl.get("/items", headers={"authorization": "Bearer " + tok}).json(), {"user": "u"})
        weak = cl.post("/token", data={"username": "u", "password": "pw"}).json()["access_token"]
        r = cl.get("/items", headers={"authorization": "Bearer " + weak})
        self.assertEqual(r.status_code, 401)
        self.assertIn('scope="items"', r.headers["www-authenticate"])
        r = cl.get("/items", headers={"authorization": "Bearer nope"})
        self.assertEqual(r.status_code, 401)
        op = app.routes[1].openapi()
        self.assertIn({"OAuth2PasswordBearer": ["items"]}, op["security"])
        self.assertEqual(op["x-veil-security-schemes"]["OAuth2PasswordBearer"]["flows"]["password"]["tokenUrl"],
                         "/token")
        form_op = app.routes[0].openapi()
        self.assertIn("application/x-www-form-urlencoded", form_op["requestBody"]["content"])


class TestJWTAudience(unittest.TestCase):
    # B-201: audience を渡さないとき、aud 付きのトークン（別のサービス向け）は拒否する
    @unittest.skipIf(jwt is None, "PyJWT is not installed")
    def test_aud_is_checked_by_default(self):
        exp = datetime.datetime.now(datetime.timezone.utc) + datetime.timedelta(minutes=5)
        tokens = security.JWT(secret="s3cret-s3cret-s3cret-s3cret-s3cret")
        other = tokens.encode({"sub": "u", "aud": "billing", "exp": exp})
        with self.assertRaises(veil.HTTPException) as e:
            tokens.decode(other)
        self.assertEqual(e.exception.status_code, 401)
        self.assertEqual(tokens.decode(tokens.encode({"sub": "u", "exp": exp}))["sub"], "u")
        mine = security.JWT(secret="s3cret-s3cret-s3cret-s3cret-s3cret", audience="api")
        self.assertEqual(mine.decode(mine.encode({"sub": "u", "aud": "api", "exp": exp}))["sub"], "u")
        with self.assertRaises(veil.HTTPException):
            mine.decode(other)
        lax = security.JWT(secret="s3cret-s3cret-s3cret-s3cret-s3cret", verify_audience=False)
        self.assertEqual(lax.decode(other)["sub"], "u")


class TestRedirects(unittest.TestCase):
    def test_is_safe_url(self):
        ok = ["/home", "/a?b=c", "https://app.example.com/x"]
        bad = ["//evil.com", "/\\evil.com", "https://evil.com", "javascript:alert(1)", "https://u:p@app.example.com/",
               "/x\r\nSet-Cookie: a=b", ""]
        for u in ok:
            self.assertTrue(redirects.is_safe_url(u, ["app.example.com"]), u)
        for u in bad:
            self.assertFalse(redirects.is_safe_url(u, ["app.example.com"]), u)
        r = redirects.safe_redirect("https://evil.com", ["app.example.com"])
        self.assertEqual(dict(r.headers)["location"], "/")
        self.assertEqual(r.status, 303)


class TestNativeHelpers(unittest.TestCase):
    def test_cors_and_static_register_options(self):
        app = veil.App()
        app.add_middleware(CORSMiddleware, allow_origins=["https://a.example"], allow_methods=["*"],
                           allow_headers=["*"], allow_credentials=True)
        self.assertEqual(app.cors_options[0]["origins"], ["https://a.example"])
        self.assertTrue(app.cors_options[0]["any_header"])
        self.assertIn("DELETE", app.cors_options[0]["methods"])
        with self.assertRaises(ValueError):
            app.add_middleware(CORSMiddleware, allow_origin_regex=".*")
        with self.assertRaises(TypeError):
            app.add_middleware(object)
        app.mount("/static", StaticFiles(directory=HERE, html=True))
        self.assertEqual(app.static_dirs[0][:3], ("/static", HERE, "index.html"))
        with self.assertRaises(RuntimeError):
            StaticFiles(directory=os.path.join(HERE, "missing"))


class Settings:
    def __init__(self):
        self.name = "prod"


class Repo:
    def __init__(self, settings: Settings):
        self.settings = settings


class TestDI(unittest.TestCase):
    def test_container_scopes_and_override(self):
        c = di.Container()
        made = []

        def make_settings():
            made.append(1)
            return Settings()

        c.singleton(Settings, make_settings)
        c.scoped(Repo)
        app = veil.App()

        @app.get("/r")
        def r(repo: Repo = di.Inject(Repo, c), s: Settings = di.Inject(Settings, c)):
            return {"name": repo.settings.name, "same": repo.settings is s}

        cl = TestClient(app)
        self.assertEqual(cl.get("/r").json(), {"name": "prod", "same": True})
        cl.get("/r")
        self.assertEqual(len(made), 1)  # singleton
        fake = Settings()
        fake.name = "test"
        with c.override(Settings, fake):
            self.assertEqual(cl.get("/r").json()["name"], "test")
        self.assertEqual(cl.get("/r").json()["name"], "prod")
        self.assertIsInstance(c.get(Repo), Repo)

    def test_generator_provider_and_late_registration(self):
        c = di.Container()
        log = []
        app = veil.App()

        class Session:
            pass

        @app.get("/s")
        def s(sess: Session = di.Inject(Session, c)):
            log.append("handler")
            return {"ok": isinstance(sess, Session)}

        def session():
            log.append("open")
            yield Session()
            log.append("close")

        c.scoped(Session, session)  # ルートより後の登録
        self.assertEqual(TestClient(app).get("/s").json(), {"ok": True})
        self.assertEqual(log, ["open", "handler", "close"])


class TestTasks(unittest.TestCase):
    def test_cron_next(self):
        sched = tasks.CronSchedule("*/15 9-17 * * 1-5")
        sched._croniter = None
        t = datetime.datetime(2026, 10, 24, 12, 0, tzinfo=datetime.timezone.utc)  # 土曜
        self.assertEqual(sched.next_after(t), datetime.datetime(2026, 10, 26, 9, 0, tzinfo=datetime.timezone.utc))
        t = datetime.datetime(2026, 10, 26, 9, 7, tzinfo=datetime.timezone.utc)
        self.assertEqual(sched.next_after(t), datetime.datetime(2026, 10, 26, 9, 15, tzinfo=datetime.timezone.utc))
        with self.assertRaises(ValueError):
            tasks.CronSchedule("61 * * * *")

    def test_every_runs_on_startup_and_stops(self):
        app = veil.App()
        t = tasks.Tasks(app)
        hits = []

        @t.every(seconds=0.05, run_at_start=True)
        async def tick():
            hits.append(time.monotonic())

        @t.background
        async def resident():
            import asyncio

            while True:
                await asyncio.sleep(1)

        import asyncio

        async def scenario():
            await app._startup()
            await asyncio.sleep(0.18)
            await app._shutdown()

        asyncio.run(scenario())
        self.assertGreaterEqual(len(hits), 3)
        self.assertEqual(t._running, [])


class TestTesting(unittest.TestCase):
    def test_override_dependency(self):
        app = veil.App()

        def real():
            return "real"

        @app.get("/")
        def root(v: str = Depends(real)):
            return v

        cl = testing.TestClient(app)
        with testing.override_dependency(app, real, lambda: "fake"):
            self.assertEqual(cl.get("/").text, "fake")
        self.assertEqual(cl.get("/").text, "real")


@unittest.skipIf(strawberry is None, "strawberry is not installed")
class TestGraphQL(unittest.TestCase):
    def test_query_mutation_sse(self):
        from veil_kit.graphql import mount

        @strawberry.type
        class Query:
            @strawberry.field
            def hello(self, name: str = "world") -> str:
                return "hello " + name

        @strawberry.type
        class Subscription:
            @strawberry.subscription
            async def count(self, to: int) -> int:
                for i in range(1, to + 1):
                    yield i

        app = veil.App()
        mount(app, "/graphql", strawberry.Schema(query=Query, subscription=Subscription))
        cl = TestClient(app)
        r = cl.post("/graphql", json={"query": '{ hello(name: "veil") }'})
        self.assertEqual(r.json(), {"data": {"hello": "hello veil"}})
        r = cl.get("/graphql", params={"query": "{ hello }"})
        self.assertEqual(r.json(), {"data": {"hello": "hello world"}})
        r = cl.post("/graphql", json=[{"query": "{ hello }"}, {"query": '{ hello(name: "b") }'}])
        self.assertEqual(len(r.json()), 2)
        r = cl.get("/graphql", headers={"accept": "text/html"})
        self.assertIn("graphiql", r.text.lower())
        r = cl.post("/graphql", json={"query": "subscription { count(to: 2) }"}, headers={"accept": "text/event-stream"})
        self.assertIn('data: {"data":{"count":1}}', r.text)
        self.assertIn("event: complete", r.text)


class TestGrpcClientShape(unittest.TestCase):
    def test_rpc_error(self):
        from veil_kit import grpc as vgrpc

        e = vgrpc.RpcError(5, "nope", (), (("k", "v"),))
        self.assertEqual(e.code().name, "NOT_FOUND")
        self.assertEqual(e.details(), "nope")
        self.assertEqual(e.trailing_metadata(), (("k", "v"),))
        try:
            import grpc
        except ImportError:
            return
        self.assertIsInstance(e, grpc.RpcError)


if __name__ == "__main__":
    unittest.main(verbosity=2)
