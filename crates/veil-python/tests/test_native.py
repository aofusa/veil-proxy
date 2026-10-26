"""サーバの中の経路（F-212）の単体テスト: ``veil._veil.NativeRequest`` と、ルートごとに前もって選ぶ呼び出し口
``Route._call`` が、純 Python の ``Request`` と ``Route.handle`` と同じ結果になること。

    python -I tests/test_native.py        # ネイティブ拡張（maturin develop）が要る。無ければスキップ
"""

import asyncio
import json
import os
import sys
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "python"))

import veil  # noqa: E402
from veil import App, HTTPException, Request, Response  # noqa: E402

try:
    from veil import _veil

    HAVE_NATIVE = True
except ImportError:
    HAVE_NATIVE = False


def native(query="", body=b"", params=None, headers=(), method="GET", path="/x"):
    return _veil.NativeRequest(method, path, query, list(headers), body, params or {}, "10.0.0.1")


def python(query="", body=b"", params=None, headers=(), method="GET", path="/x"):
    return Request(method, path, query, headers, body, params or {}, "10.0.0.1")


@unittest.skipUnless(HAVE_NATIVE, "native extension not built")
class TestNativeRequest(unittest.TestCase):
    def test_attributes_match_python_request(self):
        kw = dict(
            query="a=1&a=2&b=&c=x%20y",
            body=b'{"k": [1]}',
            params={"id": "7"},
            headers=[("User-Agent", "curl"), ("X-A", "1"), ("x-a", "2")],
            method="POST",
            path="/items/7",
        )
        n, p = native(**kw), python(**kw)
        for attr in ("method", "path", "query", "body", "params", "client_ip", "query_params"):
            self.assertEqual(getattr(n, attr), getattr(p, attr), attr)
        self.assertEqual(n.json(), p.json())
        self.assertEqual(n.text(), p.text())
        for name in ("user-agent", "X-A", "missing"):
            self.assertEqual(n.headers.get(name), p.headers.get(name))
            self.assertEqual(n.headers.getlist(name), p.headers.getlist(name))
            self.assertEqual(name in n.headers, name in p.headers)
        self.assertEqual(n.headers["USER-AGENT"], "curl")
        with self.assertRaises(KeyError):
            n.headers["missing"]
        self.assertEqual(n.headers.keys(), p.headers.keys())
        self.assertEqual(list(n.headers), list(p.headers))
        self.assertEqual(n.headers.items(), p.headers.items())
        self.assertEqual(len(n.headers), len(p.headers))
        self.assertEqual(n.headers.get("missing", "d"), "d")

    def test_isinstance_and_extra_attributes(self):
        Request.register(_veil.NativeRequest)
        veil.Headers.register(_veil.NativeHeaders)
        n = native()
        self.assertIsInstance(n, Request)
        self.assertIsInstance(n.headers, veil.Headers)
        n.user = "alice"  # ミドルウェア等が属性を足せる
        self.assertEqual(n.user, "alice")

    def test_attributes_are_cached(self):
        n = native(body=b"abc", params={"a": "1"})
        self.assertIs(n.body, n.body)
        self.assertIs(n.params, n.params)
        self.assertIs(n.query_params, n.query_params)


@unittest.skipUnless(HAVE_NATIVE, "native extension not built")
class TestFastCall(unittest.TestCase):
    """``_call`` + ``to_response``（Rust と同じ変換規則）が ``handle`` と同じ応答になる。"""

    def same(self, route, **kw):
        expect = route.handle(python(**kw))
        got = route._call(native(**kw))
        if not isinstance(got, Response):
            got = route.to_response(got)
        self.assertEqual((got.status, got.headers, got.body), (expect.status, expect.headers, expect.body))
        return got

    def setUp(self):
        self.app = App()

    def last(self):
        return self.app.routes[-1]

    def test_no_params(self):
        self.app.get("/")(lambda: "pong")
        self.same(self.last())

    def test_request_param(self):
        @self.app.get("/r")
        def f(req: Request):
            return {"ua": req.headers.get("user-agent"), "q": req.query_params}

        self.same(self.last(), query="a=1", headers=[("User-Agent", "t")])

    def test_path_str_params(self):
        @self.app.get("/u/{a}/{b}")
        def f(a, b: str):
            return {"a": a, "b": b}

        self.same(self.last(), params={"a": "x", "b": "y"})

        @self.app.get("/one/{name}")
        def g(name: str):
            return {"message": "Hello, " + name}

        self.same(self.last(), params={"name": "veil"})

    def test_typed_and_query_params(self):
        @self.app.get("/t/{n}")
        def f(n: int, flag: bool = False):
            return {"n": n, "flag": flag}

        self.same(self.last(), params={"n": "3"}, query="flag=true")
        self.same(self.last(), params={"n": "x"})  # 422

    def test_body_and_constraints(self):
        @self.app.post("/b", body_constraints={"a": {"type": "integer", "required": True}})
        def f(body: dict):
            return body

        self.same(self.last(), body=json.dumps({"a": 1}).encode())
        self.same(self.last(), body=b"{}")

    def test_async_and_exceptions(self):
        @self.app.get("/a")
        async def f():
            await asyncio.sleep(0)
            return {"ok": True}

        self.same(self.last())

        @self.app.get("/e")
        def g():
            raise HTTPException(404, "nope")

        self.same(self.last())

    def test_return_types(self):
        for value, kw in [
            ("héllo", {}),
            (b"\x00\x01", {}),
            (None, {}),
            (None, {"status_code": 201}),
            ([1, 2], {}),
            (({"a": 1}, 202), {"status_code": 201}),
            (Response("x", 418, {"X-A": "b"}, "text/x"), {}),
        ]:
            app = App()
            app.get("/", **kw)(lambda v=value: v)
            self.same(app.routes[-1])


@unittest.skipUnless(HAVE_NATIVE, "native extension not built")
class TestJsonFast(unittest.TestCase):
    """Rust の JSON 化は Python の ``_dumps`` と同じバイト列。扱わない値は None（Python に戻す）。"""

    def test_same_bytes_as_python(self):
        cases = [
            {"message": "Hello, veil"},
            {"a": [1, -2, True, False, None, "x"], "b": {"c": {"d": []}}, "e": {}},
            ["日本語", "quote\" back\\slash", "ctl\n\t\r\x01\x1f", "\u2028 ok", "emoji 🎉"],
            [9223372036854775807, -9223372036854775808],
            ("tuple", 1),
            [],
        ]
        for c in cases:
            self.assertEqual(_veil._json_fast(c), veil._dumps(c), c)

    def test_unsupported_falls_back(self):
        for c in [{"f": 1.5}, {1: "int key"}, [2 ** 70], ["\ud800"], [object()], [{"x": [1.0]}]]:
            self.assertIsNone(_veil._json_fast(c), c)


@unittest.skipUnless(HAVE_NATIVE, "native extension not built")
class TestGrpcClientCancel(unittest.TestCase):
    # B-203: 応答ヘッダの前（何も返さないサーバ）でも取消しが効き、status() が戻る
    def test_cancel_before_response_headers(self):
        import socket
        import threading

        srv = socket.socket()
        srv.bind(("127.0.0.1", 0))
        srv.listen(8)
        held = []
        stop = threading.Event()

        def accept():
            srv.settimeout(0.2)
            while not stop.is_set():
                try:
                    held.append(srv.accept()[0])  # 受けるだけで何も返さない
                except OSError:
                    pass

        t = threading.Thread(target=accept, daemon=True)
        t.start()
        try:
            ch = _veil.GrpcChannel("http://127.0.0.1:%d" % srv.getsockname()[1])
            call = ch.call("/demo.Demo/Hello")
            call.send(b"")
            out = []
            waiter = threading.Thread(target=lambda: out.append(call.status()), daemon=True)
            waiter.start()
            waiter.join(0.5)
            self.assertTrue(waiter.is_alive(), "the call should still be waiting for the server")
            call.cancel()
            waiter.join(10)
            self.assertFalse(waiter.is_alive(), "status() did not return after cancel()")
            self.assertEqual(out[0][0], 1)  # CANCELLED
            self.assertIsNone(call.recv())
        finally:
            stop.set()
            t.join(2)
            for c in held:
                c.close()
            srv.close()

    # B-203: 大きすぎる・負・NaN の timeout は panic せず ValueError
    def test_invalid_timeout(self):
        ch = _veil.GrpcChannel("http://127.0.0.1:9")
        for t in (1e300, -1.0, float("nan"), float("inf")):
            with self.assertRaises(ValueError, msg=repr(t)):
                ch.call("/demo.Demo/Hello", [], t)


if __name__ == "__main__":
    unittest.main()
