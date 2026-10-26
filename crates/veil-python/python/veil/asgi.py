"""Serving ASGI applications (FastAPI, Starlette, ...) with veil.

.. code-block:: python

    from fastapi import FastAPI
    import veil.asgi

    api = FastAPI()

    @api.get("/hello")
    async def hello():
        return {"hello": "world"}

    veil.asgi.run(api, listen_cleartext="0.0.0.0:8000")   # instead of uvicorn

or mount it next to native routes (``app.mount("/legacy", api)``). veil (Rust) parses HTTP/1.1, HTTP/2 and HTTP/3,
terminates TLS, compresses and logs; the ASGI application runs on the persistent event loop and only sees the
``receive`` / ``send`` events. Supported scopes: ``http`` (with the ``http.response.trailers`` and
``http.response.pathsend`` extensions), ``websocket`` (HTTP/1.1) and ``lifespan``.

Differences from uvicorn: the request body is read before the application is called (``[api] max_body_size``
applies), ``scheme`` is ``https`` only for HTTP/3 (or when the client sent ``x-forwarded-proto``), and the client
port is not known (``client`` is ``(ip, 0)``).
"""

# F-268: ASGI のアプリを veil の上で動かす。HTTP の解析・TLS・HTTP/2・HTTP/3 は Rust、ASGI のアプリには
# receive / send のイベントだけを渡す。

import asyncio
import traceback

__all__ = ["run", "ASGIApp"]

_HOP = frozenset((b"content-length", b"transfer-encoding", b"connection", b"keep-alive", b"upgrade"))
_HOP_STR = frozenset(h.decode() for h in _HOP)


def _scope_headers(req):
    return [(k.lower().encode("latin-1"), v.encode("latin-1")) for k, v in req.headers.items()]


def _http_version(req):
    v = getattr(req, "http_version", None) or "HTTP/1.1"
    return {"HTTP/1.1": "1.1", "HTTP/2": "2", "HTTP/3": "3"}.get(v, "1.1")


def _scheme(req, http_version, websocket=False):
    fwd = req.headers.get("x-forwarded-proto")
    if fwd:
        s = fwd.split(",")[0].strip().lower()
    else:
        s = "https" if http_version == "3" else "http"
    if websocket:
        return "wss" if s == "https" else "ws"
    return s


def _server(req):
    host = req.headers.get("host") or ""
    name, _, port = host.rpartition(":")
    if not name or not port.isdigit():
        return (host or "localhost", None)
    return (name.strip("[]"), int(port))


class ASGIApp:
    """An ASGI application mounted at ``prefix`` (created by ``App.mount``)."""

    status_code = 200
    is_async = True
    stream_body = False
    offload = True
    compress = True
    params = ()

    def __init__(self, app, prefix=""):
        """Wrap ``app`` (an ASGI 3 callable) mounted at ``prefix``."""
        self.app = app
        self.prefix = prefix.rstrip("/")
        self.state = {}
        self._lifespan = None

    # ---- HTTP ----

    def _scope(self, req):
        hv = _http_version(req)
        path = req.path
        q = req.query or ""
        return {
            "type": "http",
            "asgi": {"version": "3.0", "spec_version": "2.4"},
            "http_version": hv,
            "method": req.method,
            "scheme": _scheme(req, hv),
            "path": path,
            "raw_path": path.encode("utf-8"),
            "query_string": q.encode("latin-1"),
            "root_path": self.prefix,
            "headers": _scope_headers(req),
            "client": (req.client_ip, 0),
            "server": _server(req),
            "state": dict(self.state),
            "extensions": {"http.response.trailers": {}, "http.response.pathsend": {}},
        }

    async def _acall(self, req):
        """Run the application for one request on the event loop and return the response."""
        from . import Response, StreamingResponse

        loop = asyncio.get_running_loop()
        scope = self._scope(req)
        body = req.body
        done = loop.create_future()  # 応答の始まり（全量か、ストリーミングか）
        finished = loop.create_future()  # 応答を送り終えた（http.disconnect を返してよい）
        st = {"start": None, "body_sent": False, "queue": None, "trailers": [], "want_trailers": False}

        async def receive():
            if not st["body_sent"]:
                st["body_sent"] = True
                return {"type": "http.request", "body": bytes(body), "more_body": False}
            await asyncio.shield(finished)
            return {"type": "http.disconnect"}

        async def send(msg):
            t = msg["type"]
            if t == "http.response.start":
                if st["start"] is not None:
                    raise RuntimeError("http.response.start sent twice")
                st["start"] = msg
                st["want_trailers"] = bool(msg.get("trailers"))
            elif t == "http.response.body":
                if st["start"] is None:
                    raise RuntimeError("http.response.body before http.response.start")
                chunk = bytes(msg.get("body", b""))
                more = bool(msg.get("more_body", False))
                q = st["queue"]
                if q is None:
                    if not more and not st["want_trailers"] and not done.done():
                        done.set_result(("full", chunk))
                        return
                    q = st["queue"] = asyncio.Queue(16)
                    if not done.done():
                        done.set_result(("stream", q))
                if chunk:
                    await q.put(chunk)
                if not more and not st["want_trailers"]:
                    await q.put(None)
            elif t == "http.response.trailers":
                for k, v in msg.get("headers", ()):
                    st["trailers"].append((k.decode("latin-1") if isinstance(k, bytes) else k,
                                           v.decode("latin-1") if isinstance(v, bytes) else v))
                if not msg.get("more_trailers", False) and st["queue"] is not None:
                    await st["queue"].put(None)
            elif t == "http.response.pathsend":
                from ._aio import run_in_threadpool

                data = await run_in_threadpool(_read_file, msg["path"])
                await send({"type": "http.response.body", "body": data, "more_body": False})
            else:
                raise RuntimeError("unsupported ASGI message %r" % (t,))

        task = loop.create_task(self._run(scope, receive, send, done))
        try:
            kind, value = await done
        except BaseException:
            if not finished.done():
                finished.set_result(None)
            task.cancel()
            raise
        start = st["start"]
        status = int(start["status"])
        headers = []
        compress = True
        for k, v in start.get("headers", ()):
            k = k.decode("latin-1") if isinstance(k, bytes) else str(k)
            v = v.decode("latin-1") if isinstance(v, bytes) else str(v)
            low = k.lower()
            if low in _HOP_STR:
                continue
            if low == "content-encoding":
                compress = False
            headers.append((low, v))
        if kind == "full":
            if not finished.done():
                finished.set_result(None)
            return Response(value, status, headers, compress=compress)

        async def chunks():
            try:
                while True:
                    c = await value.get()
                    if c is None:
                        break
                    yield c
            finally:
                if not finished.done():
                    finished.set_result(None)
                if not task.done():
                    task.cancel()
                sr.trailers = st["trailers"] or None

        sr = StreamingResponse(chunks(), status, headers, compress=compress)
        return sr

    async def _run(self, scope, receive, send, done):
        try:
            await self.app(scope, receive, send)
            if not done.done():
                raise RuntimeError("the ASGI application returned without sending a response")
        except asyncio.CancelledError:
            raise
        except BaseException as e:  # noqa: BLE001  応答の前なら 500
            if not done.done():
                traceback.print_exc()
                done.set_exception(e)
            else:
                traceback.print_exc()

    def _call(self, req):
        """Run the application to completion on the calling thread (when not on the event loop)."""
        from . import _run_coro

        return _run_coro(self._full(req))

    async def _full(self, req):
        from . import StreamingResponse

        r = await self._acall(req)
        if isinstance(r, StreamingResponse):
            parts = []
            async for c in r._aiter():
                parts.append(c)
            from . import Response

            out = Response(b"".join(parts), r.status, r.headers, compress=r.compress)
            return out
        return r

    def to_response(self, r):
        """Responses are already :class:`veil.Response` objects."""
        return r

    def handle(self, req):
        """Run one request (outside the server, for tests)."""
        return self._call(req)

    def openapi_json(self):
        """ASGI applications publish their own OpenAPI documents."""
        return None

    # ---- WebSocket ----

    receive = None
    validate = False

    async def call(self, ws, req):
        """Run the application for one WebSocket connection (``ws`` is a :class:`veil.WebSocket`)."""
        hv = _http_version(req)
        q = req.query or ""
        scope = {
            "type": "websocket",
            "asgi": {"version": "3.0", "spec_version": "2.4"},
            "http_version": hv,
            "scheme": _scheme(req, hv, websocket=True),
            "path": req.path,
            "raw_path": req.path.encode("utf-8"),
            "query_string": q.encode("latin-1"),
            "root_path": self.prefix,
            "headers": _scope_headers(req),
            "client": (req.client_ip, 0),
            "server": _server(req),
            "subprotocols": ws.subprotocols,
            "state": dict(self.state),
            "extensions": {"websocket.http.response": {}},
        }
        connected = {"sent": False}

        async def receive():
            if not connected["sent"]:
                connected["sent"] = True
                return {"type": "websocket.connect"}
            from .websockets import WebSocketState

            if ws.application_state == WebSocketState.DISCONNECTED or ws.client_state == WebSocketState.DISCONNECTED:
                return {"type": "websocket.disconnect", "code": 1006}
            return await ws.receive()

        async def send(msg):
            t = msg["type"]
            if t == "websocket.accept":
                hs = [(k.decode("latin-1") if isinstance(k, bytes) else k,
                       v.decode("latin-1") if isinstance(v, bytes) else v) for k, v in msg.get("headers", ())]
                await ws.accept(msg.get("subprotocol"), hs)
            elif t == "websocket.send":
                if msg.get("text") is not None:
                    await ws.send_text(msg["text"])
                else:
                    await ws.send_bytes(msg.get("bytes") or b"")
            elif t == "websocket.close":
                await ws.close(msg.get("code", 1000), msg.get("reason"))
            elif t == "websocket.http.response.start":
                # 受け入れずに HTTP の応答で断る（本文は送らない）
                ws._n.reject(int(msg.get("status", 403)))
                from .websockets import WebSocketState

                ws.application_state = WebSocketState.DISCONNECTED
            elif t == "websocket.http.response.body":
                pass
            else:
                raise RuntimeError("unsupported ASGI message %r" % (t,))

        await self.app(scope, receive, send)

    # ---- lifespan ----

    async def startup(self):
        """Run the application's ``lifespan`` startup (ignored when the application does not support it)."""
        loop = asyncio.get_running_loop()
        inbox = asyncio.Queue()
        started = loop.create_future()
        stopped = loop.create_future()
        scope = {"type": "lifespan", "asgi": {"version": "3.0", "spec_version": "2.0"}, "state": self.state}

        async def receive():
            return await inbox.get()

        async def send(msg):
            t = msg["type"]
            if t == "lifespan.startup.complete":
                started.set_result(None)
            elif t == "lifespan.startup.failed":
                started.set_exception(RuntimeError("ASGI startup failed: %s" % msg.get("message", "")))
            elif t == "lifespan.shutdown.complete":
                stopped.set_result(None)
            elif t == "lifespan.shutdown.failed":
                stopped.set_exception(RuntimeError("ASGI shutdown failed: %s" % msg.get("message", "")))

        async def main():
            try:
                await self.app(scope, receive, send)
            except BaseException as e:  # noqa: BLE001  lifespan に対応しないアプリ
                if not started.done():
                    started.set_result("unsupported")
                elif not stopped.done():
                    stopped.set_exception(e)

        task = loop.create_task(main())
        await inbox.put({"type": "lifespan.startup"})
        r = await started
        if r == "unsupported":
            return
        self._lifespan = (task, inbox, stopped)

    async def shutdown(self):
        """Run the application's ``lifespan`` shutdown."""
        if self._lifespan is None:
            return
        task, inbox, stopped = self._lifespan
        self._lifespan = None
        await inbox.put({"type": "lifespan.shutdown"})
        try:
            await asyncio.wait_for(stopped, 30)
        finally:
            if not task.done():
                task.cancel()


def _read_file(path):
    with open(path, "rb") as f:
        return f.read()


def run(app, **kwargs):
    """Serve the ASGI application ``app`` (keyword arguments as for :meth:`veil.App.run`)."""
    from . import App

    host = App()
    host.mount("/", app)
    kwargs.setdefault("openapi", False)
    host.run(**kwargs)
