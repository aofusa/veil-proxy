"""Unit tests of the persistent event loop, WebSocket, upgrade and lifespan helpers (F-263; no native extension)."""

import asyncio
import contextlib
import json
import os
import sys
import threading
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "python"))

import veil  # noqa: E402
from veil import _aio, websockets  # noqa: E402


class FakeNativeWs:
    """ネイティブの WebSocket の代わり（受信は queue、送信は sent に溜める）。"""

    def __init__(self, inbound=()):
        self.inbound = list(inbound)
        self.sent = []
        self.accepted = None
        self.rejected = None
        self.closed = None
        self.finished = False
        self.full_once = False

    def accept(self, sub, headers):
        self.accepted = (sub, headers)

    def reject(self, status):
        if self.accepted is None and self.rejected is None:
            self.rejected = status

    def take(self):
        return self.inbound.pop(0) if self.inbound else None

    def send(self, op, data):
        if self.closed is not None:
            return 2
        if self.full_once:
            self.full_once = False
            return 1
        self.sent.append((op, data))
        return 0

    def close(self, code, reason):
        self.closed = code

    def finish(self):
        self.finished = True


def req(path="/ws", query="", headers=(), params=None):
    return veil.Request("GET", path, query, headers, b"", params or {}, "10.0.0.1")


class FakeJob:
    def __init__(self, handler_id, request, native=None, id=1):
        self.handler_id = handler_id
        self.request = request
        self.websocket = native
        self.id = id
        self.result = None
        self.failed = False


class TestWebSocket(unittest.TestCase):
    def setUp(self):
        veil._HANDLERS.clear()
        self.app = veil.App()

    def run_ws(self, route, native, request=None):
        veil._HANDLERS.append(route)

        class El:
            sockets = {}

        asyncio.run(websockets._run_ws(El(), FakeJob(len(veil._HANDLERS) - 1, request or req(), native)))

    def test_echo_and_disconnect(self):
        @self.app.websocket("/ws")
        async def ws(websocket: veil.WebSocket, room: str = "lobby"):
            await websocket.accept(subprotocol="chat.v1")
            async for t in websocket.iter_text():
                await websocket.send_text(room + ":" + t)

        n = FakeNativeWs([("text", "a"), ("text", "b"), ("close", 1000)])
        self.run_ws(self.app.websocket_routes[-1], n, req(query="room=r1"))
        self.assertEqual(n.accepted, ("chat.v1", []))
        self.assertEqual(n.sent, [(1, "r1:a"), (1, "r1:b")])
        self.assertTrue(n.finished)

    def test_no_accept_is_403_and_error_is_1011(self):
        @self.app.websocket("/a")
        async def a(websocket):
            return

        n = FakeNativeWs()
        self.run_ws(self.app.websocket_routes[-1], n)
        self.assertEqual(n.rejected, 403)

        @self.app.websocket("/b")
        async def b(websocket):
            await websocket.accept()
            raise ValueError("boom")

        n = FakeNativeWs()
        self.run_ws(self.app.websocket_routes[-1], n)
        self.assertEqual(n.closed, 1011)

        @self.app.websocket("/c")
        async def c(websocket):
            raise ValueError("before accept")

        n = FakeNativeWs()
        self.run_ws(self.app.websocket_routes[-1], n)
        self.assertEqual(n.rejected, 500)

    def test_receive_kinds_json_and_send_retry(self):
        got = {}

        @self.app.websocket("/j")
        async def j(websocket):
            await websocket.accept()
            got["json"] = await websocket.receive_json()
            got["bytes"] = await websocket.receive_bytes()
            got["msg"] = await websocket.receive()
            try:
                await websocket.receive_text()
            except veil.WebSocketDisconnect as e:
                got["code"] = e.code
            websocket._n.full_once = True
            await websocket.send_json({"ok": True})

        n = FakeNativeWs([("text", '{"x": 1}'), ("bytes", b"\x00"), ("text", "t"), ("close", 1001)])
        self.run_ws(self.app.websocket_routes[-1], n)
        self.assertEqual(got["json"], {"x": 1})
        self.assertEqual(got["bytes"], b"\x00")
        self.assertEqual(got["msg"], {"type": "websocket.receive", "text": "t"})
        self.assertEqual(got["code"], 1001)
        self.assertEqual(n.sent, [(1, '{"ok":true}')])

    def test_invalid_json_closes_1007(self):
        @self.app.websocket("/v")
        async def v(websocket):
            await websocket.accept()
            await websocket.receive_json()

        n = FakeNativeWs([("text", "{not json")])
        self.run_ws(self.app.websocket_routes[-1], n)
        self.assertEqual(n.closed, 1007)

    def test_validation_error_of_params_is_422(self):
        @self.app.websocket("/p/{n}")
        async def p(websocket, n: int):
            await websocket.accept()

        n = FakeNativeWs()
        self.run_ws(self.app.websocket_routes[-1], n, req("/p/x", params={"n": "x"}))
        self.assertEqual(n.rejected, 422)

    def test_sync_handler_rejected(self):
        with self.assertRaises(TypeError):

            @self.app.websocket("/s")
            def s(websocket):
                pass

    def test_openapi(self):
        @self.app.websocket("/chat/{room}", subprotocols=["chat.v1"], tags=["chat"], summary="Chat")
        async def chat(websocket, room: str, token: str = ""):
            """Chat room.

            Longer text."""

        op = self.app.websocket_routes[-1].openapi()
        self.assertEqual(op["summary"], "Chat")
        self.assertEqual(op["tags"], ["chat"])
        names = [(p["name"], p["in"]) for p in op["parameters"]]
        self.assertEqual(names, [("room", "path"), ("token", "query"), ("Sec-WebSocket-Protocol", "header")])
        self.assertEqual(op["x-veil-websocket"], {"subprotocols": ["chat.v1"]})
        json.loads(self.app.websocket_routes[-1].openapi_json())

    def test_endpoint_session(self):
        events = []

        @self.app.websocket("/e")
        class E(veil.WebSocketEndpoint):
            encoding = "json"

            def on_connect(self, websocket):
                events.append("connect")
                websocket.send_text("hi")

            def on_receive(self, websocket, data):
                events.append(data)

            def on_disconnect(self, websocket, code):
                events.append(code)

        route = self.app.websocket_routes[-1]
        self.assertIs(route.endpoint, E)

        class Sender:
            def __init__(self):
                self.sent = []

            def send(self, op, data):
                self.sent.append((op, data))
                return 0

            def close(self, code, reason):
                pass

        s = Sender()
        sess = route._endpoint_session(s, req())
        sess.on_connect()
        sess.on_message('{"a": 1}')
        sess.on_close(1000)
        self.assertEqual(events, ["connect", {"a": 1}, 1000])
        self.assertEqual(s.sent, [(1, "hi")])
        self.assertEqual(route.openapi()["operationId"], "E")


class TestLoop(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.el = _aio.EventLoop(99, "asyncio")
        cls.el.start()

    @classmethod
    def tearDownClass(cls):
        cls.el.stop()

    def test_run_call_complete_fail_and_stream(self):
        app = veil.App()

        @app.get("/a")
        async def a(x: int = 1):
            await asyncio.sleep(0)
            return {"x": x}

        @app.get("/b")
        async def b():
            raise RuntimeError("boom")

        @app.get("/c")
        async def c():
            async def gen():
                yield b"1"
                yield "2"

            return veil.StreamingResponse(gen(), trailers=[("x-t", "v")])

        veil._HANDLERS[:] = app.routes

        class Job(FakeJob):
            def complete(self, r):
                self.result = r

            def fail(self):
                self.failed = True

            def cancelled(self):
                self.result = "cancelled"

            def start_stream(self, sr):
                job = self
                job.chunks = []

                class W:
                    closed = False

                    def try_write(self, item):
                        job.chunks.append(item)
                        return True

                    def finish(self, trailers):
                        job.trailers = trailers

                return W()

        j = Job(0, req("/a", "x=5"))
        self.el.run_sync(_aio._run_call(self.el, j), 5)
        self.assertEqual(j.result, {"x": 5})
        j = Job(1, req("/b"))
        self.el.run_sync(_aio._run_call(self.el, j), 5)
        self.assertTrue(j.failed)
        j = Job(2, req("/c"))
        self.el.run_sync(_aio._run_call(self.el, j), 5)
        self.assertEqual(j.chunks, [b"1", "2"])
        self.assertEqual(j.trailers, [("x-t", "v")])

    def test_run_in_threadpool_and_async_protocol(self):
        async def main():
            return await veil.run_in_threadpool(lambda a, b=0: (threading.current_thread().name, a + b), 1, b=2)

        name, v = self.el.run_sync(main(), 5)
        self.assertEqual(v, 3)
        self.assertTrue(name.startswith("veil-threadpool"))

        seen = []

        class P(veil.AsyncProtocol):
            async def data_received(self, data):
                seen.append((data, threading.current_thread().name))

        orig = _aio._LOOPS[:]
        _aio._LOOPS[:] = [self.el]
        try:
            sp = veil.Response.switching_protocols("x-p", P)
            self.assertEqual(sp.status, 101)
            proto = sp._make_protocol()
            proto.connection_made(object())
            proto.data_received(b"x")
        finally:
            _aio._LOOPS[:] = orig
        self.assertEqual(seen, [(b"x", "veil-aio-99")])


class TestLifespan(unittest.TestCase):
    def test_order_and_state(self):
        order = []

        @contextlib.asynccontextmanager
        async def lifespan(app):
            order.append("enter")
            yield {"db": "pool"}
            order.append("exit")

        app = veil.App(lifespan=lifespan)

        @app.on_event("startup")
        def s1():
            order.append("startup")

        @app.on_shutdown
        async def s2():
            order.append("shutdown")

        async def both():
            # 実際と同じく同じループで（asyncio.run は終わるときに非同期ジェネレータを閉じる）
            await app._startup()
            self.assertEqual(app.state.db, "pool")
            await app._shutdown()

        asyncio.run(both())
        self.assertEqual(order, ["enter", "startup", "shutdown", "exit"])

    def test_async_mode_argument(self):
        app = veil.App()
        with self.assertRaises(ValueError):
            app.get("/x", async_mode="bogus")(lambda: None)

        @app.get("/y", async_mode="pool")
        async def y():
            return 1

        self.assertEqual(app.routes[-1].async_mode, "pool")


if __name__ == "__main__":
    unittest.main(verbosity=2)
