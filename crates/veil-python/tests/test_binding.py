"""引数束縛・検証・戻り値変換・OpenAPI 生成の単体テスト（サーバ・ネイティブ拡張は不要）。

    python -I tests/test_binding.py        # python/ を自動で sys.path に足す
pydantic が入っていれば pydantic 関連のテストも走る（無ければスキップ）。
"""

import json
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "python"))

import veil  # noqa: E402
from veil import App, HTTPException, JSONResponse, Request, Response  # noqa: E402

try:
    from pydantic import BaseModel, Field

    class Address(BaseModel):
        city: str

    class User(BaseModel):
        name: str = Field(min_length=1)
        age: int = Field(ge=0, le=120)
        address: Address = None

    HAVE_PYDANTIC = True
except ImportError:
    HAVE_PYDANTIC = False


class Base(unittest.TestCase):
    def setUp(self):
        self.app = App()

    def req(self, route, query="", body=b"", params=None, headers=()):
        r = Request("GET", "/x", query, headers, body, params or {}, "10.0.0.1")
        return route.handle(r)

    def last(self):
        return self.app.routes[-1]

    def j(self, resp):
        return json.loads(resp.body)


class TestBinding(Base):
    def test_path_and_query_conversion(self):
        @self.app.get("/items/{item_id}")
        def f(item_id: int, flag: bool = False, ratio: float = 1.0, name: str = "n"):
            return {"i": item_id, "f": flag, "r": ratio, "n": name}

        resp = self.req(self.last(), "flag=true&ratio=2.5&name=a%20b", params={"item_id": "42"})
        self.assertEqual(resp.status, 200)
        self.assertEqual(self.j(resp), {"i": 42, "f": True, "r": 2.5, "n": "a b"})
        self.assertIn(("content-type", "application/json"), resp.headers)

    def test_path_conversion_failure_422(self):
        @self.app.get("/items/{item_id}")
        def f(item_id: int):
            return {}

        resp = self.req(self.last(), params={"item_id": "abc"})
        self.assertEqual(resp.status, 422)
        d = self.j(resp)["detail"][0]
        self.assertEqual(d["loc"], ["path", "item_id"])
        self.assertEqual(d["type"], "int_parsing")

    def test_missing_required_query_422(self):
        @self.app.get("/q")
        def f(a: int, b: str = "x"):
            return {}

        resp = self.req(self.last(), "b=1")
        self.assertEqual(resp.status, 422)
        self.assertEqual(self.j(resp)["detail"], [{"loc": ["query", "a"], "msg": "Field required", "type": "missing"}])

    def test_query_conversion_failure_422(self):
        @self.app.get("/q")
        def f(a: int = 0, b: bool = False):
            return {}

        resp = self.req(self.last(), "a=x&b=maybe")
        locs = [e["loc"] for e in self.j(resp)["detail"]]
        self.assertEqual(locs, [["query", "a"], ["query", "b"]])

    def test_defaults_used_when_absent(self):
        @self.app.get("/q")
        def f(a: int = 7):
            return {"a": a}

        self.assertEqual(self.j(self.req(self.last())), {"a": 7})

    def test_request_param(self):
        @self.app.get("/r")
        def f(request: Request):
            return {
                "ua": request.headers.get("User-Agent"),
                "ct": request.headers["CONTENT-TYPE"],
                "q": request.query_params,
                "ip": request.client_ip,
                "text": request.text(),
            }

        resp = self.req(
            self.last(), "a=1&a=2&b=", body=b"hi",
            headers=[("user-agent", "curl"), ("Content-Type", "text/plain")],
        )
        self.assertEqual(self.j(resp), {"ua": "curl", "ct": "text/plain", "q": {"a": "1", "b": ""}, "ip": "10.0.0.1", "text": "hi"})

    def test_request_by_annotation(self):
        @self.app.get("/r")
        def f(r: Request):
            return r.path

        self.assertEqual(self.req(self.last()).body, b"/x")

    def test_dict_body(self):
        @self.app.post("/b")
        def f(body: dict):
            return body

        resp = self.req(self.last(), body=b'{"a": [1, 2]}')
        self.assertEqual(self.j(resp), {"a": [1, 2]})

    def test_list_annotation_body(self):
        @self.app.post("/b")
        def f(items: list):
            return {"n": len(items)}

        self.assertEqual(self.j(self.req(self.last(), body=b"[1,2,3]")), {"n": 3})

    def test_body_missing_and_invalid_json(self):
        @self.app.post("/b")
        def f(body: dict):
            return body

        r1 = self.req(self.last())
        self.assertEqual(r1.status, 422)
        self.assertEqual(self.j(r1)["detail"][0]["loc"], ["body"])
        self.assertEqual(self.j(r1)["detail"][0]["type"], "missing")
        r2 = self.req(self.last(), body=b"{bad")
        self.assertEqual(r2.status, 422)
        self.assertEqual(self.j(r2)["detail"][0]["type"], "json_invalid")

    def test_async_handler(self):
        @self.app.get("/a")
        async def f():
            return {"ok": True}

        self.assertEqual(self.j(self.req(self.last())), {"ok": True})

    def test_keyword_only_unknown_params_are_query(self):
        @self.app.get("/k")
        def f(*args, x: int = 1, **kw):
            return {"x": x}

        self.assertEqual(self.j(self.req(self.last(), "x=5")), {"x": 5})


class TestReturn(Base):
    def run_with(self, value, **kw):
        self.app = App()
        self.app.get("/", **kw)(lambda: value)
        return self.req(self.app.routes[-1])

    def test_conversions(self):
        r = self.run_with({"a": 1})
        self.assertEqual((r.status, r.body), (200, b'{"a":1}'))
        r = self.run_with([1, 2])
        self.assertEqual(r.body, b"[1,2]")
        r = self.run_with("héllo")
        self.assertIn(("content-type", "text/plain; charset=utf-8"), r.headers)
        self.assertEqual(r.body, "héllo".encode())
        r = self.run_with(b"\x00\x01")
        self.assertIn(("content-type", "application/octet-stream"), r.headers)
        r = self.run_with(None)
        self.assertEqual((r.status, r.body), (204, b""))
        r = self.run_with(({"a": 1}, 201))
        self.assertEqual(r.status, 201)
        r = self.run_with({"a": 1}, status_code=201)
        self.assertEqual(r.status, 201)
        r = self.run_with(({"a": 1}, 202), status_code=201)
        self.assertEqual(r.status, 202)
        r = self.run_with(Response("x", 418, {"X-A": "b"}, "text/x"))
        self.assertEqual((r.status, r.headers), (418, [("X-A", "b"), ("content-type", "text/x")]))
        r = self.run_with(JSONResponse({"z": 1}, 400))
        self.assertEqual((r.status, r.body), (400, b'{"z":1}'))

    def test_http_exception(self):
        @self.app.get("/e")
        def f():
            raise HTTPException(404, "nope")

        r = self.req(self.last())
        self.assertEqual((r.status, self.j(r)), (404, {"detail": "nope"}))

    def test_http_exception_default_detail(self):
        self.assertEqual(HTTPException(403).detail, "Forbidden")

    def test_unhandled_exception_propagates(self):
        @self.app.get("/e")
        def f():
            raise RuntimeError("boom")

        with self.assertRaises(RuntimeError):
            self.req(self.last())

    def test_dispatch_triple(self):
        @self.app.get("/d/{x}")
        def f(x: int):
            return {"x": x}

        veil._HANDLERS.append(self.last())
        hid = len(veil._HANDLERS) - 1
        status, headers, body = veil._dispatch(hid, "GET", "/d/3", "", [], b"", {"x": "3"}, "1.2.3.4")
        self.assertEqual((status, body), (200, b'{"x":3}'))


class TestConstraints(Base):
    CONS = {
        "name": {"type": "string", "minLength": 1, "maxLength": 5, "pattern": "[a-z]+", "required": True},
        "age": {"type": "integer", "minimum": 0, "maximum": 120},
        "score": {"type": "number", "exclusiveMinimum": 0, "exclusiveMaximum": 1},
        "role": {"enum": ["a", "b"]},
        "required": ["age"],
    }

    def setUp(self):
        super().setUp()

        @self.app.post("/u", body_constraints=self.CONS)
        def f(body: dict):
            return body

        self.route = self.last()

    def post(self, obj):
        return self.req(self.route, body=json.dumps(obj).encode())

    def types(self, resp):
        return [(e["loc"][-1], e["type"]) for e in self.j(resp)["detail"]]

    def test_ok(self):
        r = self.post({"name": "abc", "age": 30, "score": 0.5, "role": "a"})
        self.assertEqual(r.status, 200)

    def test_each_rule(self):
        self.assertEqual(self.types(self.post({"name": "abc", "age": -1})), [("age", "greater_than_equal")])
        self.assertEqual(self.types(self.post({"name": "abc", "age": 121})), [("age", "less_than_equal")])
        self.assertEqual(self.types(self.post({"name": "", "age": 1})), [("name", "string_too_short"), ("name", "string_pattern_mismatch")])
        self.assertEqual(self.types(self.post({"name": "abcdef", "age": 1})), [("name", "string_too_long")])
        self.assertEqual(self.types(self.post({"name": "ABC", "age": 1})), [("name", "string_pattern_mismatch")])
        self.assertEqual(self.types(self.post({"name": "abc", "age": 1, "score": 0})), [("score", "greater_than")])
        self.assertEqual(self.types(self.post({"name": "abc", "age": 1, "score": 1})), [("score", "less_than")])
        self.assertEqual(self.types(self.post({"name": "abc", "age": 1, "role": "z"})), [("role", "enum")])

    def test_required_and_types(self):
        self.assertEqual(sorted(self.types(self.post({}))), [("age", "missing"), ("name", "missing")])
        self.assertEqual(self.types(self.post({"name": "abc", "age": "3"})), [("age", "integer_type")])
        self.assertEqual(self.types(self.post({"name": "abc", "age": True})), [("age", "integer_type")])
        r = self.post([1])
        self.assertEqual(r.status, 422)
        self.assertEqual(self.j(r)["detail"][0]["type"], "dict_type")

    def test_constraints_without_body_param(self):
        @self.app.post("/v", body_constraints={"age": {"minimum": 0}})
        def g():
            return "ok"

        r = self.req(self.last(), body=b'{"age": -3}')
        self.assertEqual(r.status, 422)
        self.assertEqual(self.req(self.last(), body=b'{"age": 3}').status, 200)

    def test_openapi_reflects_constraints(self):
        op = self.route.openapi()
        schema = op["requestBody"]["content"]["application/json"]["schema"]
        self.assertEqual(schema["type"], "object")
        self.assertEqual(schema["properties"]["age"], {"type": "integer", "minimum": 0, "maximum": 120})
        self.assertEqual(schema["properties"]["name"]["maxLength"], 5)
        self.assertNotIn("required", schema["properties"]["name"])
        self.assertEqual(sorted(schema["required"]), ["age", "name"])
        self.assertIn("422", op["responses"])


class TestOpenAPI(Base):
    def test_params_and_docs(self):
        @self.app.get("/items/{item_id}", tags=["t"], status_code=200)
        def read_item(item_id: int, q: str = "x", n: float = 1.5, must: bool = None):
            """Read one item.

            More text."""
            return {}

        op = self.last().openapi()
        self.assertEqual(op["operationId"], "read_item")
        self.assertEqual(op["summary"], "Read one item.")
        self.assertEqual(op["description"], "More text.")
        self.assertEqual(op["tags"], ["t"])
        ps = {p["name"]: p for p in op["parameters"]}
        self.assertEqual(ps["item_id"], {"name": "item_id", "in": "path", "required": True, "schema": {"type": "integer"}})
        self.assertEqual(ps["q"]["schema"], {"type": "string", "default": "x"})
        self.assertFalse(ps["q"]["required"])
        self.assertEqual(ps["n"]["schema"]["type"], "number")
        self.assertEqual(ps["must"]["schema"]["type"], "boolean")
        json.loads(self.last().openapi_json())

    def test_no_params_no_422(self):
        @self.app.get("/p")
        def ping():
            return "pong"

        self.assertEqual(list(self.last().openapi()["responses"]), ["200"])

    def test_status_code_in_responses(self):
        @self.app.post("/c", status_code=201)
        def c():
            return {}

        self.assertIn("201", self.last().openapi()["responses"])

    def test_default_app_decorators(self):
        @veil.get("/mod")
        def m():
            return {}

        self.assertEqual(veil._default_app.routes[-1].path, "/mod")
        veil._default_app.routes.clear()

    def test_toml_literal(self):
        self.assertEqual(veil._toml_literal(True), "true")
        self.assertEqual(veil._toml_literal(4), "4")
        self.assertEqual(veil._toml_literal('a"b'), '"a\\"b"')
        self.assertEqual(veil._toml_literal(["x", 1]), '["x", 1]')
        self.assertEqual(veil._toml_literal({"k": 1}), '{"k" = 1}')

    # F-260: 待ち受けの列（Listener・辞書）はネイティブの Config.add_listener に 1 個ずつ渡る
    def test_listener_values(self):
        calls = []

        class FakeConfig:
            def add_listener(self, listen, tls, **kw):
                calls.append((listen, tls, kw))

        for value in (
            veil.Listener("0.0.0.0:8443", tls=True, http3=True, name="https"),
            {"listen": "0.0.0.0:8082", "allow_http": False},
        ):
            veil.Listener.from_value(value).add_to(FakeConfig())
        self.assertEqual(
            calls,
            [
                ("0.0.0.0:8443", True, {"http3": True, "name": "https"}),
                ("0.0.0.0:8082", False, {"allow_http": False}),
            ],
        )
        with self.assertRaises(TypeError):
            veil.Listener("0.0.0.0:1", alow_http=True)
        with self.assertRaises(ValueError):
            veil.Listener.from_value({"tls": True})
        with self.assertRaises(TypeError):
            veil.Listener.from_value("0.0.0.0:1")


@unittest.skipUnless(HAVE_PYDANTIC, "pydantic 未インストール")
class TestPydantic(Base):
    def setUp(self):
        super().setUp()

        @self.app.post("/users", status_code=201)
        def create(user: User) -> User:
            return user

        self.route = self.last()

    def test_ok_roundtrip(self):
        r = self.req(self.route, body=b'{"name":"al","age":3}')
        self.assertEqual(r.status, 201)
        self.assertEqual(self.j(r)["name"], "al")
        self.assertIn(("content-type", "application/json"), r.headers)

    def test_validation_error_shape(self):
        r = self.req(self.route, body=b'{"name":"","age":200,"address":{}}')
        self.assertEqual(r.status, 422)
        errs = self.j(r)["detail"]
        locs = sorted(tuple(e["loc"]) for e in errs)
        self.assertEqual(locs, [("body", "address", "city"), ("body", "age"), ("body", "name")])
        for e in errs:
            self.assertEqual(set(e), {"loc", "msg", "type"})

    def test_invalid_json_and_missing(self):
        r = self.req(self.route, body=b"{")
        self.assertEqual(r.status, 422)
        self.assertEqual(self.j(r)["detail"][0]["type"], "json_invalid")
        r = self.req(self.route)
        self.assertEqual(self.j(r)["detail"][0]["type"], "missing")

    def test_openapi_components(self):
        op = self.route.openapi()
        ref = op["requestBody"]["content"]["application/json"]["schema"]
        self.assertEqual(ref, {"$ref": "#/components/schemas/User"})
        comps = op["x-veil-components"]
        self.assertIn("User", comps)
        self.assertIn("Address", comps)
        self.assertEqual(comps["User"]["properties"]["age"]["maximum"], 120)
        self.assertNotIn("$defs", comps["User"])
        self.assertEqual(op["responses"]["201"]["content"]["application/json"]["schema"], ref)

    def test_model_return_and_tuple(self):
        @self.app.get("/m")
        def m():
            return User(name="z", age=1), 202

        r = self.req(self.last())
        self.assertEqual(r.status, 202)
        self.assertEqual(self.j(r)["age"], 1)


# F-266
def _multipart(fields, boundary="XyZ"):
    out = b""
    for name, value, filename, ctype in fields:
        disp = 'form-data; name="%s"' % name
        if filename is not None:
            disp += '; filename="%s"' % filename
        out += b"--" + boundary.encode() + b"\r\ncontent-disposition: " + disp.encode() + b"\r\n"
        if ctype:
            out += b"content-type: " + ctype.encode() + b"\r\n"
        out += b"\r\n" + value + b"\r\n"
    return out + b"--" + boundary.encode() + b"--\r\n"


class TestCompressFlags(Base):
    # F-270: ルートと応答の圧縮の可否（圧縮そのものは Rust 側。ここでは Rust へ渡す値を確かめる）
    def test_route_and_response_flags(self):
        @self.app.get("/a")
        def a():
            return {"x": 1}

        @self.app.get("/b", compress=False)
        def b():
            return {"x": 1}

        self.assertEqual([r.compress for r in self.app.routes], [True, False])
        self.assertTrue(veil.Response(b"x").compress)
        self.assertFalse(veil.Response(b"x", compress=False).compress)
        self.assertFalse(veil.JSONResponse({"a": 1}, compress=False).compress)
        self.assertFalse(veil.StreamingResponse(iter([b"x"]), compress=False).compress)
        self.assertTrue(veil.EventSourceResponse(iter([])).compress)


class TestForms(Base):
    MP = (("content-type", "multipart/form-data; boundary=XyZ"),)

    def setUp(self):
        super().setUp()

        @self.app.post("/profile")
        async def profile(
            name: str = veil.Form(),
            age: int = veil.Form(0),
            tags: list[str] = veil.Form([]),
            avatar: veil.UploadFile = veil.File(),
            extra: veil.UploadFile = None,
        ):
            data = await avatar.read()
            return {
                "name": name,
                "age": age,
                "tags": tags,
                "file": avatar.filename,
                "ct": avatar.content_type,
                "len": len(data),
                "size": avatar.size,
                "extra": extra.filename if extra else None,
            }

        self.route = self.last()

    def test_multipart_fields_and_file(self):
        body = _multipart(
            [
                ("name", b"al", None, None),
                ("age", b"7", None, None),
                ("tags", b"a", None, None),
                ("tags", b"b", None, None),
                ("avatar", b"\x00\x01png", "a.png", "image/png"),
            ]
        )
        r = self.req(self.route, body=body, headers=self.MP)
        self.assertEqual(r.status, 200, r.body)
        self.assertEqual(
            self.j(r),
            {"name": "al", "age": 7, "tags": ["a", "b"], "file": "a.png", "ct": "image/png", "len": 5, "size": 5, "extra": None},
        )

    def test_missing_and_invalid(self):
        r = self.req(self.route, body=_multipart([("age", b"x", None, None)]), headers=self.MP)
        self.assertEqual(r.status, 422)
        locs = [e["loc"] for e in self.j(r)["detail"]]
        self.assertIn(["body", "name"], locs)
        self.assertIn(["body", "age"], locs)
        self.assertIn(["body", "avatar"], locs)

    def test_malformed_is_400(self):
        r = self.req(self.route, body=b"--XyZ\r\nbroken", headers=self.MP)
        self.assertEqual(r.status, 400)

    def test_urlencoded_and_bytes_file(self):
        @self.app.post("/login")
        def login(user: str = veil.Form(), remember: bool = veil.Form(False)):
            return {"user": user, "remember": remember}

        r = self.req(
            self.last(), body=b"user=a%20b&remember=on", headers=(("content-type", "application/x-www-form-urlencoded"),)
        )
        self.assertEqual(self.j(r), {"user": "a b", "remember": True})

        @self.app.post("/raw")
        def raw(doc: bytes = veil.File(), docs: list[veil.UploadFile] = veil.File([])):
            return {"n": len(doc), "docs": [d.filename for d in docs]}

        body = _multipart([("doc", b"abc", "d.txt", None), ("docs", b"1", "x", None), ("docs", b"2", "y", None)])
        r = self.req(self.last(), body=body, headers=self.MP)
        self.assertEqual(self.j(r), {"n": 3, "docs": ["x", "y"]})

    def test_openapi_multipart(self):
        op = self.route.openapi()
        schema = op["requestBody"]["content"]["multipart/form-data"]["schema"]
        self.assertEqual(schema["properties"]["avatar"], {"type": "string", "format": "binary"})
        self.assertEqual(schema["properties"]["tags"]["type"], "array")
        self.assertEqual(schema["properties"]["age"], {"type": "integer", "default": 0})
        self.assertEqual(schema["required"], ["name", "avatar"])
        self.assertIn("422", op["responses"])


if __name__ == "__main__":
    unittest.main(verbosity=2)
