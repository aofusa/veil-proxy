"""WebSocket endpoints (``@app.websocket``), with the same names as Starlette / FastAPI.

The handshake, frames, Ping / Pong, UTF-8 checks and size limits are handled in Rust by the network worker; Python
receives assembled messages (``str`` / ``bytes``) on the persistent event loop.
"""

import asyncio
import collections
import inspect
import json
import traceback

__all__ = ["WebSocket", "WebSocketDisconnect", "WebSocketEndpoint", "WebSocketState", "Address"]

Address = collections.namedtuple("Address", ["host", "port"])


class WebSocketState:
    """Connection states (``websocket.client_state`` / ``application_state``)."""

    CONNECTING = 0
    CONNECTED = 1
    DISCONNECTED = 2


class WebSocketDisconnect(Exception):
    """Raised by ``receive_*`` when the client closed the connection (``code`` is the close code)."""

    def __init__(self, code=1000, reason=None):
        super().__init__(code, reason)
        self.code = code
        self.reason = reason or ""


class _State:
    """Free-form attributes (``websocket.state``)."""


# ネイティブの send の戻り値（crates/veil-python/src/aio.rs の NativeWebSocket::send）
_SENT, _FULL, _CLOSED = 0, 1, 2


class WebSocket:
    """One WebSocket connection, passed to an ``@app.websocket`` handler.

    Call :meth:`accept` first (until then the handshake is pending; returning without accepting answers 403).
    """

    def __init__(self, native, request, route):
        self._n = native
        self._req = request
        self._route = route
        self._event = asyncio.Event()
        self.state = _State()
        self.client_state = WebSocketState.CONNECTING
        self.application_state = WebSocketState.CONNECTING
        self.path_params = dict(request.params)

    # ---- 要求の情報 ----

    @property
    def headers(self):
        """The request headers of the handshake."""
        return self._req.headers

    @property
    def query_params(self):
        """The query parameters of the handshake (first value wins)."""
        return self._req.query_params

    @property
    def url(self):
        """The request path and query (``/ws?room=1``)."""
        q = self._req.query
        return self._req.path + ("?" + q if q else "")

    @property
    def client(self):
        """``(host, port)`` of the client (the port is not known and is ``None``)."""
        return Address(self._req.client_ip, None)

    @property
    def subprotocols(self):
        """The subprotocols the client offered (``Sec-WebSocket-Protocol``)."""
        v = self._req.headers.get("sec-websocket-protocol") or ""
        return [p.strip() for p in v.split(",") if p.strip()]

    # ---- ハンドシェイク ----

    async def accept(self, subprotocol=None, headers=None):
        """Answer the handshake with 101 (``subprotocol`` is sent as ``Sec-WebSocket-Protocol``)."""
        if self.application_state != WebSocketState.CONNECTING:
            raise RuntimeError("WebSocket is already accepted or closed")
        hs = []
        if headers:
            items = headers.items() if isinstance(headers, dict) else headers
            hs = [(str(k), str(v)) for k, v in items]
        self._n.accept(subprotocol, hs)
        self.application_state = WebSocketState.CONNECTED
        self.client_state = WebSocketState.CONNECTED

    # ---- 受信 ----

    def _wake(self):
        self._event.set()

    async def _next(self):
        while True:
            m = self._n.take()
            if m is not None:
                if m[0] == "close":
                    self.client_state = WebSocketState.DISCONNECTED
                return m
            self._event.clear()
            await self._event.wait()

    async def receive(self):
        """The next message as an ASGI-style dict (``websocket.receive`` or ``websocket.disconnect``)."""
        self._check_accepted()
        if self.client_state == WebSocketState.DISCONNECTED:
            raise RuntimeError("WebSocket is disconnected")
        kind, value = await self._next()
        if kind == "text":
            return {"type": "websocket.receive", "text": value}
        if kind == "bytes":
            return {"type": "websocket.receive", "bytes": value}
        return {"type": "websocket.disconnect", "code": value}

    async def _receive_kind(self, want):
        self._check_accepted()
        if self.client_state == WebSocketState.DISCONNECTED:
            raise WebSocketDisconnect(1006)
        kind, value = await self._next()
        if kind == "close":
            raise WebSocketDisconnect(value)
        if kind != want:
            raise TypeError("expected a %s message, got %s" % (want, kind))
        return value

    async def receive_text(self):
        """The next text message (``WebSocketDisconnect`` when the client closed)."""
        return await self._receive_kind("text")

    async def receive_bytes(self):
        """The next binary message (``WebSocketDisconnect`` when the client closed)."""
        return await self._receive_kind("bytes")

    async def receive_json(self, mode="text"):
        """The next message decoded as JSON (``mode`` is ``"text"`` or ``"binary"``).

        With ``@app.websocket(..., receive=Model, validate=True)`` the value is validated against the pydantic
        model and the connection is closed with 1007 when it does not match.
        """
        raw = await self._receive_kind("text" if mode == "text" else "bytes")
        try:
            value = json.loads(raw)
        except ValueError:
            await self._invalid()
        model = self._route.receive
        if self._route.validate and model is not None:
            try:
                model.model_validate(value)
            except Exception:  # noqa: BLE001  pydantic.ValidationError を import せずに判定
                await self._invalid()
        return value

    async def _invalid(self):
        await self.close(1007)
        raise WebSocketDisconnect(1007)

    async def iter_text(self):
        """Text messages until the client closes."""
        try:
            while True:
                yield await self.receive_text()
        except WebSocketDisconnect:
            pass

    async def iter_bytes(self):
        """Binary messages until the client closes."""
        try:
            while True:
                yield await self.receive_bytes()
        except WebSocketDisconnect:
            pass

    async def iter_json(self):
        """JSON messages until the client closes."""
        try:
            while True:
                yield await self.receive_json()
        except WebSocketDisconnect:
            pass

    # ---- 送信 ----

    async def _send(self, opcode, data):
        self._check_accepted()
        while True:
            r = self._n.send(opcode, data)
            if r == _SENT:
                return
            if r == _CLOSED:
                self.application_state = WebSocketState.DISCONNECTED
                raise WebSocketDisconnect(1006)
            # 送信キューが一杯（16 MiB）: ワーカーが書き出すのを待つ
            await asyncio.sleep(0.001)

    async def send_text(self, data):
        """Send a text message (queued; written by the network worker, several at once when they pile up)."""
        await self._send(1, data)

    async def send_bytes(self, data):
        """Send a binary message."""
        await self._send(2, bytes(data))

    async def send_json(self, data, mode="text"):
        """Send ``data`` encoded as JSON (``mode`` is ``"text"`` or ``"binary"``)."""
        from . import _dumps

        b = _dumps(data)
        await (self._send(1, b.decode("utf-8")) if mode == "text" else self._send(2, b))

    async def send(self, message):
        """Send an ASGI-style message (``websocket.send`` with ``text`` / ``bytes``, ``websocket.close``, ``websocket.accept``)."""
        t = message.get("type")
        if t == "websocket.accept":
            await self.accept(message.get("subprotocol"), message.get("headers"))
        elif t == "websocket.send":
            if message.get("text") is not None:
                await self.send_text(message["text"])
            else:
                await self.send_bytes(message.get("bytes") or b"")
        elif t == "websocket.close":
            await self.close(message.get("code", 1000), message.get("reason"))
        else:
            raise ValueError("unsupported message type %r" % (t,))

    async def close(self, code=1000, reason=None):
        """Start the closing handshake (before :meth:`accept`, rejects the handshake with 403)."""
        if self.application_state == WebSocketState.CONNECTING:
            self._n.reject(403)
        elif self.application_state == WebSocketState.CONNECTED:
            self._n.close(int(code), reason or "")
        self.application_state = WebSocketState.DISCONNECTED

    def _check_accepted(self):
        if self.application_state == WebSocketState.CONNECTING:
            raise RuntimeError('WebSocket is not connected. Need to call "accept" first.')


async def _run_ws(el, job):
    """Run an ``@app.websocket`` handler for one connection on the event loop."""
    from . import _HANDLERS

    native = job.websocket
    ws = None
    # B-207: WebSocket の生成で失敗しても、ワーカーが承認を待ち続けないよう try の中で作る（500 で断る）
    try:
        route = _HANDLERS[job.handler_id]
        ws = WebSocket(native, job.request, route)
        el.sockets[job.id] = ws
        await route.call(ws, job.request)
    except WebSocketDisconnect:
        pass
    except Exception:  # noqa: BLE001  接続を 1011（受け入れ前なら 500）で閉じる
        traceback.print_exc()
        if ws is None or ws.application_state == WebSocketState.CONNECTING:
            native.reject(500)
        else:
            native.close(1011, "")
        if ws is not None:
            ws.application_state = WebSocketState.DISCONNECTED
    finally:
        el.sockets.pop(job.id, None)
        if ws is not None and ws.application_state == WebSocketState.CONNECTING:
            # accept せずに戻った（Starlette と同じく 403）
            native.reject(403)
        native.finish()


class WebSocketEndpoint:
    """Class-based WebSocket handler with the method names of Starlette's ``WebSocketEndpoint``.

    The connection is accepted before :meth:`on_connect`; the callbacks run one at a time per connection on the
    handler pool (plain ``def``) or on the event loop (``async def``). ``encoding`` is ``"text"``, ``"bytes"``,
    ``"json"`` or ``None`` (``str`` or ``bytes`` as received). ``websocket`` here has ``send_text``,
    ``send_bytes``, ``send_json``, ``close``, ``client``, ``headers``, ``path_params`` and ``state``.
    """

    encoding = None

    def on_connect(self, websocket):
        """Called once the connection is open."""

    def on_receive(self, websocket, data):
        """Called with each message (decoded according to ``encoding``)."""

    def on_disconnect(self, websocket, close_code):
        """Called when the connection closed."""


class _SyncWebSocket:
    """The ``websocket`` passed to :class:`WebSocketEndpoint` callbacks (sending never waits)."""

    def __init__(self, sender, request):
        self._s = sender
        self._req = request
        self.state = _State()
        self.path_params = dict(request.params)

    @property
    def headers(self):
        return self._req.headers

    @property
    def query_params(self):
        return self._req.query_params

    @property
    def client(self):
        return Address(self._req.client_ip, None)

    def _send(self, opcode, data):
        r = self._s.send(opcode, data)
        if r == _CLOSED:
            raise WebSocketDisconnect(1006)
        if r == _FULL:
            raise RuntimeError("WebSocket send queue is full")

    def send_text(self, data):
        """Send a text message."""
        self._send(1, data)

    def send_bytes(self, data):
        """Send a binary message."""
        self._send(2, bytes(data))

    def send_json(self, data, mode="text"):
        """Send ``data`` encoded as JSON."""
        from . import _dumps

        b = _dumps(data)
        self._send(1, b.decode("utf-8")) if mode == "text" else self._send(2, b)

    def close(self, code=1000, reason=None):
        """Start the closing handshake."""
        self._s.close(int(code), reason or "")

    async def accept(self, *a, **kw):
        """No-op (the connection is already accepted); kept for Starlette-style ``on_connect``."""


class _EndpointSession:
    """Drives one :class:`WebSocketEndpoint` instance (called from Rust on the handler pool)."""

    def __init__(self, cls, sender, request):
        self.ep = cls()
        self.ws = _SyncWebSocket(sender, request)

    def _call(self, name, *args):
        f = getattr(self.ep, name)
        r = f(self.ws, *args)
        if inspect.iscoroutine(r):
            from . import _aio

            el = _aio.first()
            if el is None:
                raise RuntimeError("async WebSocketEndpoint callbacks need the event loop")
            el.run_sync(r)

    def on_connect(self):
        self._call("on_connect")

    def on_message(self, data):
        enc = self.ep.encoding
        if enc == "text" and isinstance(data, bytes):
            # Starlette と同じく、期待と違う種類のメッセージは 1003（Unsupported Data）で閉じる
            self.ws.close(1003)
            return
        if enc == "bytes" and isinstance(data, str):
            data = data.encode("utf-8")
        elif enc == "json":
            data = json.loads(data)
        self._call("on_receive", data)

    def on_close(self, code):
        self._call("on_disconnect", code)
