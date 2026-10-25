"""FastAPI-style Python bindings for the veil API server library.

The HTTP server, TLS, HTTP/2, HTTP/3 and the OpenAPI endpoints run in Rust (veil, on io_uring);
Python only supplies handler functions. The native extension ``veil._veil`` is imported lazily
by :func:`run` / :func:`shutdown` (argument binding, validation and OpenAPI generation are pure
Python and can be tested without the extension).

.. code-block:: python

    import veil

    @veil.get("/hello/{name}")
    def hello(name: str, greeting: str = "Hello"):
        return {"message": f"{greeting}, {name}"}

    veil.run(listen_cleartext="127.0.0.1:8000")
"""

import abc
import asyncio
import http
import threading
import inspect
import json
import re
import typing
import urllib.parse

from ._aio import run_in_threadpool
from . import _deps, _forms
from ._forms import File, Form, UploadFile
from .params import (
    BackgroundTasks,
    Body,
    Cookie,
    Depends,
    Header,
    Path,
    Query,
    Security,
    SecurityScopes,
    _ParamMarker,
)
from .routing import APIRouter
from .websockets import WebSocket, WebSocketDisconnect, WebSocketEndpoint, WebSocketState

__all__ = [
    "App",
    "Request",
    "Response",
    "JSONResponse",
    "HTMLResponse",
    "PlainTextResponse",
    "RedirectResponse",
    "FileResponse",
    "HTTPException",
    "Headers",
    "StreamingResponse",
    "EventSourceResponse",
    "Protocol",
    "AsyncProtocol",
    "Listener",
    "Form",
    "File",
    "UploadFile",
    "Query",
    "Path",
    "Header",
    "Cookie",
    "Body",
    "Depends",
    "Security",
    "SecurityScopes",
    "BackgroundTasks",
    "APIRouter",
    "WebSocket",
    "WebSocketDisconnect",
    "WebSocketEndpoint",
    "WebSocketState",
    "run_in_threadpool",
    "route",
    "get",
    "post",
    "put",
    "delete",
    "patch",
    "websocket",
    "on_startup",
    "on_shutdown",
    "on_event",
    "State",
    "SwitchingProtocols",
    "WebSocketRoute",
    "run",
    "shutdown",
]

_EMPTY = inspect.Parameter.empty
_JSON_CT = "application/json"


# --------------------------------------------------------------------------
# リクエスト・レスポンス
# --------------------------------------------------------------------------


class Headers(metaclass=abc.ABCMeta):
    """Case-insensitive, read-only request headers (repeated headers keep their arrival order).

    Inside the server a Rust-backed ``veil._veil.NativeHeaders`` with the same methods is passed.
    """

    def __init__(self, items=()):
        """Build headers from an iterable of ``(name, value)`` pairs."""
        self._items = [(str(k), str(v)) for k, v in items]

    def get(self, name, default=None):
        """Return the first value of header ``name`` (case-insensitive), or ``default``."""
        low = name.lower()
        for k, v in self._items:
            if k.lower() == low:
                return v
        return default

    def getlist(self, name):
        """Return all values of header ``name`` in arrival order."""
        low = name.lower()
        return [v for k, v in self._items if k.lower() == low]

    def __getitem__(self, name):
        """Return the first value of header ``name``; raise ``KeyError`` if absent."""
        v = self.get(name)
        if v is None:
            raise KeyError(name)
        return v

    def __contains__(self, name):
        """Return True if header ``name`` is present."""
        return self.get(name) is not None

    def __iter__(self):
        """Iterate over the distinct header names."""
        return iter(self.keys())

    def __len__(self):
        """Return the number of header lines (duplicates counted)."""
        return len(self._items)

    def keys(self):
        """Return the distinct header names (the first spelling seen)."""
        seen = []
        for k, _ in self._items:
            if k.lower() not in [s.lower() for s in seen]:
                seen.append(k)
        return seen

    def items(self):
        """Return all ``(name, value)`` pairs in arrival order."""
        return list(self._items)

    def __repr__(self):
        return "Headers(%r)" % (self._items,)


class Request(metaclass=abc.ABCMeta):
    """The request passed to a handler.

    Inside the server a Rust-backed ``veil._veil.NativeRequest`` (same attributes and methods; an
    attribute is created only when it is read) is passed, and ``isinstance(req, veil.Request)`` is true.
    """

    def __init__(self, method="GET", path="/", query="", headers=(), body=b"", params=None, client_ip=""):
        """Build a request directly (useful in unit tests)."""
        self.method = method
        self.path = path
        self.query = query
        self.headers = headers if isinstance(headers, Headers) else Headers(headers)
        self.body = body
        self.params = dict(params or {})
        self.client_ip = client_ip
        self._query_params = None
        self._cookies = None

    # F-268: HTTP の版（サーバの中では NativeRequest が実際の版を返す）
    http_version = "HTTP/1.1"

    @property
    def cookies(self):
        """The request cookies as a dict (``Cookie`` headers; a repeated name keeps the last value)."""
        if self._cookies is None:
            self._cookies = _parse_cookies(self.headers.getlist("cookie"))
        return self._cookies

    @property
    def query_params(self):
        """Query string as a dict (the first value of a repeated key)."""
        if self._query_params is None:
            self._query_params = _query_params(self.query)
        return self._query_params

    def json(self):
        """Parse the body as JSON."""
        return json.loads(self.body)

    def text(self):
        """Return the body decoded as UTF-8 (undecodable bytes are replaced)."""
        return self.body.decode("utf-8", errors="replace")

    def stream(self):
        """Iterate over the body in the order it arrives.

        In the server this yields each received chunk on routes declared with ``stream_body=True``;
        elsewhere it yields the whole body once.
        """
        if self.body:
            yield self.body


def _parse_cookies(values):
    out = {}
    for header in values:
        for pair in header.split(";"):
            pair = pair.strip()
            if not pair:
                continue
            k, sep, v = pair.partition("=")
            if not sep:
                k, v = "", k
            v = v.strip()
            if len(v) >= 2 and v[0] == v[-1] == '"':
                v = v[1:-1]
            out[k.strip()] = v
    return out


def _cookies_of(req):
    """The cookies of a request (``veil.Request`` or the native one)."""
    c = getattr(req, "cookies", None)
    if c is None:
        c = _parse_cookies(req.headers.getlist("cookie"))
    return c


def _query_params(query):
    """Return the query string as a dict (the first value wins for repeated keys); shared by ``Request.query_params`` and ``NativeRequest``."""
    parsed = urllib.parse.parse_qs(query, keep_blank_values=True)
    return {k: v[0] for k, v in parsed.items()}


_LOOPS = threading.local()


def _run_coro(coro):
    """Run an ``async def`` handler on a per-thread event loop (created once per thread, not per request)."""
    loop = getattr(_LOOPS, "loop", None)
    if loop is None or loop.is_closed():
        loop = asyncio.new_event_loop()
        _LOOPS.loop = loop
    return loop.run_until_complete(coro)


def _json_default(o):
    if hasattr(o, "model_dump"):
        return o.model_dump(mode="json")
    if hasattr(o, "isoformat"):
        return o.isoformat()
    return str(o)


def _dumps(obj):
    return json.dumps(obj, separators=(",", ":"), ensure_ascii=False, default=_json_default).encode("utf-8")


class Response:
    """A raw response. ``body`` is ``bytes`` or ``str``; ``media_type`` sets ``content-type`` if no such header is given.

    ``compress=False`` (or setting ``compress = False`` on an instance or a subclass) keeps the server from
    compressing it even when the route has ``[route.compression]`` or ``[api] compression`` on.
    """

    # 応答の圧縮を許すか（F-270）
    compress = True
    # F-268: 送り終えた後に呼ぶもの（``BackgroundTasks`` 等。サーバはハンドラ用プールで呼ぶ）
    background = None

    def __init__(self, body=b"", status=200, headers=None, media_type=None, compress=True, background=None,
                 status_code=None, content=None):
        """Create a response from a body, status, optional headers (dict or pairs) and media type.

        ``status_code`` and ``content`` are FastAPI's names for ``status`` and ``body``; ``background`` is called
        after the response has been sent (for example a :class:`BackgroundTasks`).
        """
        if content is not None:
            body = content
        if status_code is not None:
            status = status_code
        if background is not None:
            self.background = background
        if not compress:
            self.compress = False
        if isinstance(body, str):
            body = body.encode("utf-8")
        self.body = bytes(body)
        self.status = int(status)
        if headers is None:
            self.headers = []
        elif isinstance(headers, dict):
            self.headers = [(str(k), str(v)) for k, v in headers.items()]
        else:
            self.headers = [(str(k), str(v)) for k, v in headers]
        if media_type is not None and not any(k.lower() == "content-type" for k, _ in self.headers):
            self.headers.append(("content-type", media_type))

    def _triple(self):
        return (self.status, self.headers, self.body)

    @property
    def status_code(self):
        """Same as ``status`` (FastAPI's name)."""
        return self.status

    @status_code.setter
    def status_code(self, v):
        self.status = int(v)

    def set_cookie(self, key, value="", max_age=None, expires=None, path="/", domain=None, secure=False,
                   httponly=False, samesite="lax", partitioned=False):
        """Add a ``Set-Cookie`` header (the arguments of Starlette's ``Response.set_cookie``).

        ``expires`` is seconds from now or a ``datetime``; ``samesite`` is ``"lax"``, ``"strict"``, ``"none"`` or
        ``None``.
        """
        self.headers.append(("set-cookie", _set_cookie_value(
            key, value, max_age, expires, path, domain, secure, httponly, samesite, partitioned)))

    def delete_cookie(self, key, path="/", domain=None, secure=False, httponly=False, samesite="lax"):
        """Add a ``Set-Cookie`` header that removes the cookie ``key``."""
        self.set_cookie(key, "", 0, 0, path, domain, secure, httponly, samesite)

    @staticmethod
    def switching_protocols(protocol, factory, headers=None):
        """Answer ``101 Switching Protocols`` to ``protocol`` (HTTP/1.1 only) and serve the connection with ``factory()``.

        ``factory`` returns a :class:`Protocol` (callbacks on the handler pool) or an :class:`AsyncProtocol`
        (``async def`` callbacks on the event loop). The request must ask for the upgrade
        (``Connection: upgrade`` and ``Upgrade``); otherwise veil answers 400.
        """
        return SwitchingProtocols(protocol, factory, headers)


def _set_cookie_value(key, value, max_age, expires, path, domain, secure, httponly, samesite, partitioned=False):
    # F-268: Starlette の set_cookie と同じ形（値は http.cookies の規則で引用する）
    import email.utils
    import http.cookies

    c = http.cookies.SimpleCookie()
    c[key] = value
    m = c[key]
    if max_age is not None:
        m["max-age"] = int(max_age)
    if expires is not None:
        if hasattr(expires, "timestamp"):
            m["expires"] = email.utils.formatdate(expires.timestamp(), usegmt=True)
        else:
            import time

            m["expires"] = email.utils.formatdate(time.time() + int(expires), usegmt=True)
    if path is not None:
        m["path"] = path
    if domain is not None:
        m["domain"] = domain
    if secure:
        m["secure"] = True
    if httponly:
        m["httponly"] = True
    if samesite is not None:
        if str(samesite).lower() not in ("strict", "lax", "none"):
            raise ValueError("samesite must be 'strict', 'lax' or 'none'")
        m["samesite"] = str(samesite).capitalize() if str(samesite).lower() != "none" else "None"
    out = c.output(header="").strip()
    if partitioned:
        out += "; Partitioned"
    return out


def _aiter_sync(agen):
    """Wrap an async iterator as a synchronous iterator driven by the thread's event loop."""
    it = agen.__aiter__()
    while True:
        try:
            yield _run_coro(it.__anext__())
        except StopAsyncIteration:
            return


class StreamingResponse(Response):
    """A response whose body is sent chunk by chunk.

    ``content`` is an iterator, generator or async iterator of ``bytes`` / ``str``. In the server chunks
    are sent to the client as they are produced (iterated on a handler-pool thread). ``trailers`` is a
    list of ``(name, value)`` pairs sent after the body.
    """

    def __init__(self, content, status=200, headers=None, media_type=None, trailers=None, compress=True,
                 background=None, status_code=None):
        """Create a streaming response from ``content`` (see the class documentation)."""
        super().__init__(b"", status, headers, media_type, compress, background, status_code)
        self.content = content
        self.trailers = list(trailers) if trailers else None

    def _iter(self):
        c = self.content
        if hasattr(c, "__aiter__"):
            return _aiter_sync(c)
        return iter(c)

    async def _aiter(self):
        # 常駐のイベントループで送るとき（非同期のイテレータはループの上でそのまま回す）
        c = self.content
        if hasattr(c, "__aiter__"):
            async for x in c:
                yield x
        else:
            for x in c:
                yield x

    @property
    def body(self):
        """The whole body joined into ``bytes`` (for unit tests and ``offload=False`` routes; the server streams instead)."""
        if self._body is None:
            self._body = b"".join(x.encode("utf-8") if isinstance(x, str) else bytes(x) for x in self._iter())
        return self._body

    @body.setter
    def body(self, v):
        self._body = None if v == b"" else v


def _sse_event(item):
    if isinstance(item, dict) and set(item) <= {"event", "data", "id", "retry"} and "data" in item:
        out = []
        for k in ("event", "id", "retry"):
            if k in item:
                out.append("%s: %s\n" % (k, item[k]))
        data = item["data"]
        if not isinstance(data, str):
            data = _dumps(data).decode()
        out.extend("data: %s\n" % line for line in data.split("\n"))
        return "".join(out) + "\n"
    if not isinstance(item, str):
        item = _dumps(item).decode()
    return "".join("data: %s\n" % line for line in item.split("\n")) + "\n"


class EventSourceResponse(StreamingResponse):
    """Server-Sent Events (``text/event-stream``).

    Each item is a ``str`` (a ``data:`` line), a JSON-serialisable value, or a dict with the keys
    ``event``, ``data``, ``id`` and ``retry``.
    """

    def __init__(self, content, status=200, headers=None, background=None, status_code=None):
        """Create an SSE response from an iterable or generator of events."""
        super().__init__(content, status, headers, "text/event-stream", background=background,
                         status_code=status_code)
        hs = [k.lower() for k, _ in self.headers]
        if "cache-control" not in hs:
            self.headers.append(("cache-control", "no-cache"))

    def _iter(self):
        return (_sse_event(x) for x in super()._iter())

    async def _aiter(self):
        async for x in super()._aiter():
            yield _sse_event(x)


def _stream_of(result, status):
    """Turn a generator or async generator returned by a handler into a streaming response (called from Rust); otherwise return ``None``."""
    if inspect.isgenerator(result) or inspect.isasyncgen(result):
        return StreamingResponse(result, status, media_type="text/plain; charset=utf-8")
    return None


class Protocol:
    """Handler base class for an L4 TCP listener (same method names as ``asyncio.Protocol``).

    One instance is created per connection; the callbacks run one at a time on a handler-pool thread.
    ``self.transport.write`` is valid only inside a callback.
    """

    def connection_made(self, transport):
        """Called when a connection is accepted; the default stores ``transport``."""
        self.transport = transport

    def data_received(self, data):
        """Called with each chunk of bytes received from the peer."""
        pass

    def connection_lost(self, exc):
        """Called when the connection is closed."""
        pass


class AsyncProtocol(Protocol):
    """Like :class:`Protocol`, but ``data_received`` and ``connection_lost`` may be ``async def`` (run on the event loop).

    The callbacks still run one at a time per connection; ``self.transport.write`` is valid while one runs.
    """


class _AsyncProtocolShim:
    """Runs the ``async def`` callbacks of an :class:`AsyncProtocol` on the event loop (called on the handler pool)."""

    def __init__(self, proto):
        self.proto = proto

    def _run(self, r):
        if inspect.iscoroutine(r):
            from . import _aio

            el = _aio.first()
            if el is None:
                raise RuntimeError("AsyncProtocol needs the event loop")
            el.run_sync(r)

    def connection_made(self, transport):
        self._run(self.proto.connection_made(transport))

    def data_received(self, data):
        self._run(self.proto.data_received(data))

    def connection_lost(self, exc):
        self._run(self.proto.connection_lost(exc))


class SwitchingProtocols(Response):
    """``101 Switching Protocols`` returned by :meth:`Response.switching_protocols`."""

    def __init__(self, protocol, factory, headers=None):
        super().__init__(b"", 101, headers)
        self.protocol = str(protocol)
        self._factory = factory

    def _make_protocol(self):
        # Rust（ハンドラ用プール）から接続ごとに呼ぶ
        p = self._factory()
        return _AsyncProtocolShim(p) if isinstance(p, AsyncProtocol) else p


class JSONResponse(Response):
    """A response whose body is ``content`` encoded as JSON (``application/json``)."""
    def __init__(self, content=None, status=200, headers=None, compress=True, background=None, status_code=None,
                 media_type=None):
        """Create a JSON response from a JSON-serialisable ``content``."""
        super().__init__(_dumps(content), status, headers, media_type or _JSON_CT, compress, background,
                         status_code)


class HTMLResponse(Response):
    """A ``text/html`` response."""

    def __init__(self, content="", status=200, headers=None, compress=True, background=None, status_code=None):
        """Create an HTML response from a ``str`` or ``bytes``."""
        super().__init__(content, status, headers, "text/html; charset=utf-8", compress, background, status_code)


class PlainTextResponse(Response):
    """A ``text/plain`` response."""

    def __init__(self, content="", status=200, headers=None, compress=True, background=None, status_code=None):
        """Create a plain-text response from a ``str`` or ``bytes``."""
        super().__init__(content, status, headers, "text/plain; charset=utf-8", compress, background, status_code)


class RedirectResponse(Response):
    """A redirect to ``url`` (``307`` by default, as in Starlette)."""

    def __init__(self, url, status_code=307, headers=None, background=None):
        """Redirect to ``url`` with ``status_code``."""
        super().__init__(b"", status_code, headers, background=background)
        self.headers.append(("location", urllib.parse.quote(str(url), safe=":/%#?=@[]!$&'()*+,;")))


class FileResponse(StreamingResponse):
    """A file sent in chunks (``content-type`` from the file name; ``filename`` adds ``content-disposition``).

    For directories of static files, ``app.serve_dir`` (or ``veil_kit.staticfiles.StaticFiles``) serves them from
    the server with ``sendfile`` instead.
    """

    chunk_size = 64 * 1024

    def __init__(self, path, status_code=200, headers=None, media_type=None, filename=None, background=None,
                 content_disposition_type="attachment"):
        """Send the file at ``path``."""
        import mimetypes
        import os

        self.path = os.fspath(path)
        if media_type is None:
            media_type = mimetypes.guess_type(filename or self.path)[0] or "application/octet-stream"
        super().__init__(self._chunks(), status_code, headers, media_type, background=background)
        if not os.path.isfile(self.path):
            raise RuntimeError("File at path %s does not exist." % self.path)
        if filename is not None:
            quoted = urllib.parse.quote(filename)
            if quoted != filename:
                cd = "%s; filename*=utf-8''%s" % (content_disposition_type, quoted)
            else:
                cd = '%s; filename="%s"' % (content_disposition_type, filename)
            self.headers.append(("content-disposition", cd))

    def _chunks(self):
        with open(self.path, "rb") as f:
            while True:
                b = f.read(self.chunk_size)
                if not b:
                    return
                yield b


class HTTPException(Exception):
    """Raise to return an error response: ``raise HTTPException(404, "not found")`` gives ``{"detail": "not found"}``."""

    def __init__(self, status_code, detail=None, headers=None):
        """Create an exception with ``status_code``, an optional ``detail`` (default: the HTTP reason phrase) and extra ``headers``."""
        if detail is None:
            try:
                detail = http.HTTPStatus(status_code).phrase
            except ValueError:
                detail = ""
        super().__init__(status_code, detail)
        self.status_code = status_code
        self.detail = detail
        self.headers = headers


class _ValidationFailed(Exception):
    def __init__(self, errors):
        super().__init__(errors)
        self.errors = errors


def _err(loc, msg, typ):
    return {"loc": list(loc), "msg": msg, "type": typ}


# --------------------------------------------------------------------------
# 型注釈の扱い（pydantic は import しない。メソッドの有無で判定する）
# --------------------------------------------------------------------------


def _is_model(ann):
    return (
        isinstance(ann, type)
        and hasattr(ann, "model_validate_json")
        and hasattr(ann, "model_json_schema")
    )


def _unwrap_optional(ann):
    """Return ``(X, True)`` for ``Optional[X]`` and ``(ann, False)`` otherwise."""
    if typing.get_origin(ann) is typing.Union:
        args = [a for a in typing.get_args(ann) if a is not type(None)]
        if len(args) == 1 and len(typing.get_args(ann)) == 2:
            return args[0], True
    return ann, False


def _is_container(ann):
    return ann in (dict, list) or typing.get_origin(ann) in (dict, list)


def _scalar_schema(ann):
    if ann is bool:
        return {"type": "boolean"}
    if ann is int:
        return {"type": "integer"}
    if ann is float:
        return {"type": "number"}
    return {"type": "string"}


_BOOL_TRUE = ("true", "1", "yes", "on", "t", "y")
_BOOL_FALSE = ("false", "0", "no", "off", "f", "n")


def _convert_scalar(value, ann):
    """Convert a string according to the annotation (int/float/bool/str); raises ``ValueError(msg, type)`` on failure."""
    if ann is int:
        try:
            return int(value)
        except ValueError:
            raise ValueError("Input should be a valid integer, unable to parse string as an integer", "int_parsing")
    if ann is float:
        try:
            return float(value)
        except ValueError:
            raise ValueError("Input should be a valid number, unable to parse string as a number", "float_parsing")
    if ann is bool:
        low = value.lower()
        if low in _BOOL_TRUE:
            return True
        if low in _BOOL_FALSE:
            return False
        raise ValueError("Input should be a valid boolean, unable to interpret input", "bool_parsing")
    return value


def _resolve_hints(func):
    try:
        return typing.get_type_hints(func)
    except Exception:
        return {}


_STR_ANN = {"int": int, "float": float, "bool": bool, "str": str, "dict": dict, "list": list}


class _Param:
    __slots__ = (
        "name", "kind", "ann", "model", "default", "required", "alias", "many", "sub", "hidden", "rules", "marker",
        "slow",
    )

    def __init__(self, name, kind, ann, model, default, alias=None, many=False):
        self.name = name
        # request / websocket / path / body / query / form / file、F-268: header / cookie / depends / background /
        # response / scopes
        self.kind = kind
        self.ann = ann
        self.model = model
        self.default = default
        self.required = default is _EMPTY
        # F-266: フォームのフィールド名（既定は引数名）と、繰り返しを list で受けるか
        self.alias = alias or name
        self.many = many
        # F-268: 依存（Dependant）・ルートの dependencies（引数に渡さない）・検証の規則・引数の印
        self.sub = None
        self.hidden = False
        self.rules = None
        self.marker = None
        # 汎用の束縛（_bind_one）を通す（印・ヘッダ・Cookie）
        self.slow = False


def _form_marker(p, extra):
    """The ``Form()`` / ``File()`` marker of a parameter (its default or an ``Annotated`` metadata), if any."""
    if isinstance(p.default, _forms._FormMarker):
        return p.default
    if typing.get_origin(extra) is typing.Annotated:
        for m in extra.__metadata__:
            if isinstance(m, _forms._FormMarker):
                return m
    return None


def _form_param(name, base, marker, default):
    # F-266: list[X] は繰り返し、UploadFile は File() を付けなくてもファイル
    many = typing.get_origin(base) is list
    item = typing.get_args(base)[0] if many and typing.get_args(base) else (str if many else base)
    is_file = item is UploadFile or (marker is not None and marker.file)
    if marker is not None:
        default = _EMPTY if marker.default is ... else marker.default
    return _Param(
        name, "file" if is_file else "form", item, None, default, marker.alias if marker else None, many
    )


def _di_marker(p, extra):
    """The ``Depends()`` / ``Query()``-style marker of a parameter (its default or an ``Annotated`` metadata)."""
    if isinstance(p.default, (_ParamMarker, Depends)):
        return p.default
    if typing.get_origin(extra) is typing.Annotated:
        for m in extra.__metadata__:
            if isinstance(m, (_ParamMarker, Depends)):
                return m
    return None


def _is_sub(base, cls):
    return isinstance(base, type) and issubclass(base, cls)


def _hints_of(func):
    # クラス（依存として呼ぶ）は __init__ の型注釈、呼び出し可能なインスタンスは __call__ の型注釈
    target = func
    if inspect.isclass(func):
        target = func.__init__
    elif not (inspect.isfunction(func) or inspect.ismethod(func)) and hasattr(func, "__call__"):
        target = func.__call__
    hints = _resolve_hints(target)
    try:
        extras = typing.get_type_hints(target, include_extras=True)
    except Exception:
        extras = {}
    return hints, extras


def _dependant(call, path_names, overrides, use_cache=True, scopes=()):
    # F-268: 依存の木の 1 節（差し替えは木を作る時に当てる）
    real = overrides.get(call, call) if overrides else call
    params = _build_params(real, path_names, overrides, scopes)
    return _deps.Dependant(real, params, use_cache, scopes)


def _marked_param(name, base, optional, m, p):
    # F-268: Query()・Path()・Header()・Cookie()・Body() の付いた引数
    if p.default is m or p.default is _EMPTY:
        default = _EMPTY if m.default is ... else m.default
    else:
        default = p.default  # Annotated[int, Query(ge=1)] = 3
    if optional and default is _EMPTY and m.where != "path":
        default = None if m.default is not ... else _EMPTY
    where = m.where
    if where == "body":
        out = _Param(name, "body", base, base if _is_model(base) else None, default, m.alias)
    else:
        many = typing.get_origin(base) in (list, tuple, set)
        item = typing.get_args(base)[0] if many and typing.get_args(base) else (str if many else base)
        alias = m.alias
        if where == "header" and alias is None:
            alias = name.replace("_", "-") if m.convert_underscores else name
        out = _Param(name, where, item, None, default, alias, many)
    out.rules = m.rules or None
    out.marker = m
    out.slow = True
    return out


def _build_params(func, path_names, overrides=None, scopes=()):
    hints, extras = _hints_of(func)
    out = []
    try:
        sig = inspect.signature(func)
    except (TypeError, ValueError):
        return out
    for name, p in sig.parameters.items():
        if p.kind in (p.VAR_POSITIONAL, p.VAR_KEYWORD):
            continue
        ann = hints.get(name, p.annotation)
        if isinstance(ann, str):
            ann = _STR_ANN.get(ann, _EMPTY)
        base, optional = _unwrap_optional(ann)
        extra = extras.get(name, p.annotation)
        dm = _di_marker(p, extra)
        if isinstance(dm, Depends):
            call = dm.dependency if dm.dependency is not None else base
            if call is _EMPTY or not callable(call):
                raise TypeError("Depends() for parameter %r needs a dependency or a callable annotation" % name)
            q = _Param(name, "depends", base, None, _EMPTY)
            q.sub = _dependant(call, path_names, overrides, dm.use_cache, tuple(scopes) + tuple(dm.scopes))
            out.append(q)
            continue
        if _is_sub(base, BackgroundTasks):
            out.append(_Param(name, "background", base, None, _EMPTY))
            continue
        if _is_sub(base, SecurityScopes):
            out.append(_Param(name, "scopes", base, None, _EMPTY))
            continue
        if base is Response:
            out.append(_Param(name, "response", base, None, _EMPTY))
            continue
        if isinstance(dm, _ParamMarker):
            out.append(_marked_param(name, base, optional, dm, p))
            continue
        marker = _form_marker(p, extra)
        uploads = base is UploadFile or (typing.get_origin(base) is list and typing.get_args(base) == (UploadFile,))
        if marker is not None or uploads:
            default = p.default
            if optional and default is _EMPTY:
                default = None
            out.append(_form_param(name, base, marker, default))
        elif name in ("req", "request") or (isinstance(base, type) and issubclass(base, Request)):
            out.append(_Param(name, "request", base, None, p.default))
        elif name == "websocket" or (isinstance(base, type) and issubclass(base, WebSocket)):
            out.append(_Param(name, "websocket", base, None, p.default))
        elif name in path_names:
            out.append(_Param(name, "path", base, None, p.default))
        elif _is_model(base):
            out.append(_Param(name, "body", base, base, p.default))
        elif name == "body" or _is_container(base):
            out.append(_Param(name, "body", base, None, p.default))
        else:
            out.append(_Param(name, "query", base, None, p.default))
    return out


# --------------------------------------------------------------------------
# body_constraints（pydantic 無しの検証と OpenAPI 反映）
# --------------------------------------------------------------------------

_TYPE_CHECKS = {
    "string": lambda v: isinstance(v, str),
    "integer": lambda v: isinstance(v, int) and not isinstance(v, bool),
    "number": lambda v: isinstance(v, (int, float)) and not isinstance(v, bool),
    "boolean": lambda v: isinstance(v, bool),
    "array": lambda v: isinstance(v, list),
    "object": lambda v: isinstance(v, dict),
    "null": lambda v: v is None,
}


def _is_num(v):
    return isinstance(v, (int, float)) and not isinstance(v, bool)


def _field_rules(rules):
    """Rules to publish in OpenAPI (without the field-level ``required``)."""
    return {k: v for k, v in rules.items() if k != "required"}


def validate_constraints(obj, constraints):
    """Validate a dict body against ``body_constraints`` and return a FastAPI-style list of errors (empty if valid)."""
    errors = []
    if not isinstance(obj, dict):
        return [_err(["body"], "Input should be a valid dictionary or object", "dict_type")]
    top_required = constraints.get("required")
    if isinstance(top_required, (list, tuple)):
        for f in top_required:
            if f not in obj:
                errors.append(_err(["body", f], "Field required", "missing"))
    for field, rules in constraints.items():
        if field == "required" and isinstance(rules, (list, tuple)):
            continue
        if not isinstance(rules, dict):
            continue
        if field not in obj:
            if rules.get("required") is True:
                errors.append(_err(["body", field], "Field required", "missing"))
            continue
        errors.extend(_check_value(obj[field], rules, ["body", field]))
    return errors


def _check_value(v, rules, loc):
    errs = []
    t = rules.get("type")
    if t is not None:
        check = _TYPE_CHECKS.get(t)
        if check is not None and not check(v):
            return [_err(loc, "Input should be a valid %s" % t, "%s_type" % t)]
    if "enum" in rules and v not in rules["enum"]:
        errs.append(_err(loc, "Input should be one of %s" % (", ".join(repr(e) for e in rules["enum"]),), "enum"))
    if _is_num(v):
        if "minimum" in rules and v < rules["minimum"]:
            errs.append(_err(loc, "Input should be greater than or equal to %s" % rules["minimum"], "greater_than_equal"))
        if "maximum" in rules and v > rules["maximum"]:
            errs.append(_err(loc, "Input should be less than or equal to %s" % rules["maximum"], "less_than_equal"))
        if "exclusiveMinimum" in rules and v <= rules["exclusiveMinimum"]:
            errs.append(_err(loc, "Input should be greater than %s" % rules["exclusiveMinimum"], "greater_than"))
        if "exclusiveMaximum" in rules and v >= rules["exclusiveMaximum"]:
            errs.append(_err(loc, "Input should be less than %s" % rules["exclusiveMaximum"], "less_than"))
    if isinstance(v, str):
        if "minLength" in rules and len(v) < rules["minLength"]:
            errs.append(_err(loc, "String should have at least %d characters" % rules["minLength"], "string_too_short"))
        if "maxLength" in rules and len(v) > rules["maxLength"]:
            errs.append(_err(loc, "String should have at most %d characters" % rules["maxLength"], "string_too_long"))
        if "pattern" in rules and re.search(rules["pattern"], v) is None:
            errs.append(_err(loc, "String should match pattern '%s'" % rules["pattern"], "string_pattern_mismatch"))
    return errs


def constraints_schema(constraints):
    """Convert ``body_constraints`` to an OpenAPI JSON Schema (object)."""
    props = {}
    required = []
    top = constraints.get("required")
    if isinstance(top, (list, tuple)):
        required.extend(top)
    for field, rules in constraints.items():
        if field == "required" and isinstance(rules, (list, tuple)):
            continue
        if not isinstance(rules, dict):
            continue
        props[field] = _field_rules(rules)
        if rules.get("required") is True and field not in required:
            required.append(field)
    for f in required:
        props.setdefault(f, {})
    schema = {"type": "object", "properties": props}
    if required:
        schema["required"] = required
    return schema


# --------------------------------------------------------------------------
# ルート（引数束縛・戻り値変換・OpenAPI）
# --------------------------------------------------------------------------

_PATH_NAME_RE = re.compile(r"\{\*?([^}/:]+)(?::[^}]*)?\}")
# F-268: 依存の解決（_deps）を通す引数の種類
_DI_KINDS = frozenset(("depends", "background", "response", "scopes"))


def _status_phrase(code):
    try:
        return http.HTTPStatus(int(code)).phrase
    except ValueError:
        return "Response"


def _model_errors(exc, prefix):
    out = []
    for e in exc.errors():
        out.append(_err(list(prefix) + list(e.get("loc", ())), str(e.get("msg", "")), str(e.get("type", "value_error"))))
    return out


def _is_validation_error(exc):
    return type(exc).__name__ == "ValidationError" and hasattr(exc, "errors")


class Route:
    """One registered route. ``handle()`` is independent of Rust and can be unit-tested."""

    def __init__(
        self,
        func,
        path,
        methods,
        offload,
        body_constraints,
        tags,
        summary,
        status_code,
        stream_body=False,
        async_mode=None,
        compress=True,
        dependencies=None,
        response_model=None,
        responses=None,
        include_in_schema=True,
        response_class=None,
        deprecated=None,
        description=None,
        operation_id=None,
        overrides=None,
        response_model_exclude_none=False,
    ):
        self.func = func
        # F-268: FastAPI のルートの引数（dependencies・response_model 等）と app.dependency_overrides
        self.dependencies = list(dependencies or [])
        for d in self.dependencies:
            if not isinstance(d, Depends):
                raise TypeError("dependencies must be Depends(...) values")
        self.response_model = response_model
        self.response_model_exclude_none = response_model_exclude_none
        self.responses = dict(responses or {})
        self.include_in_schema = include_in_schema
        self.response_class = response_class
        self.deprecated = deprecated
        self.description = description
        self.operation_id = operation_id
        self._overrides = overrides
        self._adapter = None
        # 応答を圧縮してよいか（F-270。``[route.compression]``・``[api] compression``）
        self.compress = bool(compress)
        # async def の実行先（None = [api] python_async_mode の既定、"loop" = 常駐のループ、"pool" = 旧来）
        if async_mode not in (None, "loop", "pool"):
            raise ValueError('async_mode must be "loop" or "pool"')
        self.async_mode = async_mode
        self.stream_body = stream_body
        self.path = path
        self.methods = [m.upper() for m in methods]
        self.offload = offload
        self.body_constraints = body_constraints
        self.tags = list(tags) if tags else []
        self.summary = summary
        self.status_code = status_code
        self.path_names = _PATH_NAME_RE.findall(path)
        self._build()
        self.is_async = inspect.iscoroutinefunction(func)
        self._hints = _resolve_hints(func)
        self.uses_di = (
            bool(self.dependencies)
            or response_model is not None
            or response_class is not None
            or any(p.kind in _DI_KINDS for p in self.params)
        )
        self._call = self._make_call()

    def _build(self):
        """Build the parameters and the dependency tree (again after ``app.dependency_overrides`` changed)."""
        ov = self._overrides
        names = set(self.path_names)
        params = _build_params(self.func, names, ov)
        hidden = []
        for i, d in enumerate(self.dependencies):
            call = d.dependency
            if call is None:
                raise TypeError("dependencies=[Depends(...)] needs a dependency")
            q = _Param("__veil_dep_%d" % i, "depends", None, None, _EMPTY)
            q.sub = _dependant(call, names, ov, d.use_cache, tuple(d.scopes))
            q.hidden = True
            hidden.append(q)
        self.params = params
        # ルートの dependencies は引数より先に解決する（FastAPI と同じ）
        self._root = _deps.Dependant(self.func, hidden + params)
        self._ov_version = ov.version if ov is not None else 0

    def _tree(self):
        ov = self._overrides
        if ov is not None and ov.version != self._ov_version:
            self._build()
        return self._root

    # ---- 引数束縛 ----

    def bind(self, req):
        """Build the function arguments from the request; validation errors raise an internal exception that becomes a 422 response."""
        kwargs = {}
        errors = []
        qp = None
        body_checked = False
        form = None
        state = None
        for p in self.params:
            if p.slow:
                if state is None:
                    state = {}
                self._bind_one(p, req, kwargs, errors, state)
                if p.kind == "body":
                    body_checked = True
                continue
            if p.kind == "request":
                kwargs[p.name] = req
            elif p.kind == "path":
                raw = req.params.get(p.name)
                if raw is None:
                    errors.append(_err(["path", p.name], "Field required", "missing"))
                    continue
                try:
                    kwargs[p.name] = _convert_scalar(raw, p.ann)
                except ValueError as e:
                    errors.append(_err(["path", p.name], e.args[0], e.args[1]))
            elif p.kind == "query":
                if qp is None:
                    qp = req.query_params
                if p.name in qp:
                    try:
                        kwargs[p.name] = _convert_scalar(qp[p.name], p.ann)
                    except ValueError as e:
                        errors.append(_err(["query", p.name], e.args[0], e.args[1]))
                elif p.required:
                    errors.append(_err(["query", p.name], "Field required", "missing"))
                # 既定値があれば関数側の既定値に任せる
            elif p.kind == "body":
                body_checked = True
                self._bind_body(p, req, kwargs, errors)
            elif p.kind in ("form", "file"):
                if form is None:
                    form = self._form_parts(req)
                self._bind_form(p, form, kwargs, errors)
        if self.body_constraints and not body_checked:
            self._bind_body(None, req, kwargs, errors)
        if errors:
            raise _ValidationFailed(errors)
        return kwargs

    def _bind_one(self, p, req, kwargs, errors, state):
        """Bind one parameter (handlers with markers or headers, and the parameters of dependencies)."""
        k = p.kind
        if k == "request":
            kwargs[p.name] = req
            return
        if k == "websocket":
            ws = state.get("websocket")
            if ws is not None:
                kwargs[p.name] = ws
            return
        if k == "body":
            if p.marker is not None and p.marker.embed:
                self._bind_embedded(p, req, kwargs, errors, state)
            else:
                state["body_checked"] = True
                self._bind_body(p, req, kwargs, errors)
            return
        if k in ("form", "file"):
            form = state.get("form")
            if form is None:
                form = state["form"] = self._form_parts(req)
            self._bind_form(p, form, kwargs, errors)
            return
        loc = k
        if k == "path":
            raw = req.params.get(p.alias)
            values = None if raw is None else [raw]
        elif k == "query":
            if p.many:
                qs = state.get("qs")
                if qs is None:
                    qs = state["qs"] = urllib.parse.parse_qs(req.query, keep_blank_values=True)
                values = qs.get(p.alias)
            else:
                qp = req.query_params
                values = [qp[p.alias]] if p.alias in qp else None
        elif k == "header":
            if p.many:
                values = req.headers.getlist(p.alias) or None
            else:
                v = req.headers.get(p.alias)
                values = None if v is None else [v]
        elif k == "cookie":
            c = state.get("cookies")
            if c is None:
                c = state["cookies"] = _cookies_of(req)
            v = c.get(p.alias)
            values = None if v is None else [v]
        else:
            return
        if values is None:
            if p.required:
                errors.append(_err([loc, p.alias], "Field required", "missing"))
            elif p.default is not _EMPTY:
                kwargs[p.name] = list(p.default) if isinstance(p.default, (list, tuple)) and p.many else p.default
            return
        out = []
        for raw in values:
            try:
                v = _convert_scalar(raw, p.ann)
            except ValueError as e:
                errors.append(_err([loc, p.alias], e.args[0], e.args[1]))
                return
            if p.rules:
                errs = _check_value(v, p.rules, [loc, p.alias])
                if errs:
                    errors.extend(errs)
                    return
            out.append(v)
        kwargs[p.name] = out if p.many else out[0]

    def _bind_embedded(self, p, req, kwargs, errors, state):
        # F-268: Body(embed=True) と複数の Body: 本文の JSON の 1 キー
        if "json" not in state:
            body = req.body
            if not body.strip():
                state["json"] = None
            else:
                try:
                    state["json"] = json.loads(body)
                except (ValueError, UnicodeDecodeError) as e:
                    state["json"] = None
                    errors.append(_err(["body"], "JSON decode error: %s" % e, "json_invalid"))
                    return
        obj = state["json"]
        if not isinstance(obj, dict) or p.alias not in obj:
            if p.required:
                errors.append(_err(["body", p.alias], "Field required", "missing"))
            elif p.default is not _EMPTY:
                kwargs[p.name] = p.default
            return
        v = obj[p.alias]
        if p.model is not None:
            try:
                v = p.model.model_validate(v)
            except Exception as e:  # noqa: BLE001
                if _is_validation_error(e):
                    errors.extend(_model_errors(e, ["body", p.alias]))
                    return
                raise
        elif p.rules:
            errs = _check_value(v, p.rules, ["body", p.alias])
            if errs:
                errors.extend(errs)
                return
        kwargs[p.name] = v

    @staticmethod
    def _form_parts(req):
        # F-266: 名前ごとのパートの一覧（本文が壊れている・大きすぎるときは 400・413）
        try:
            parts = _forms.parts_of(req)
        except _forms.FormError as e:
            raise HTTPException(e.status, e.message)
        by_name = {}
        for part in parts:
            by_name.setdefault(part.name, []).append(part)
        return by_name

    @staticmethod
    def _bind_form(p, form, kwargs, errors):
        found = form.get(p.alias, [])
        if p.kind == "file":
            found = [x for x in found if x.is_file()]
        else:
            found = [x for x in found if not x.is_file() or p.ann is bytes]
        if not found:
            if p.required:
                errors.append(_err(["body", p.alias], "Field required", "missing"))
            elif p.default is not _EMPTY:
                kwargs[p.name] = p.default
            return
        values = []
        for part in found if p.many else found[:1]:
            if p.kind == "file" and p.ann is UploadFile:
                values.append(part.upload())
                continue
            data = part.content()
            if p.ann is bytes:
                values.append(data if isinstance(data, bytes) else data.encode("utf-8"))
                continue
            if isinstance(data, bytes):
                try:
                    data = data.decode("utf-8")
                except UnicodeDecodeError:
                    errors.append(_err(["body", p.alias], "Input should be a valid string", "string_unicode"))
                    return
            try:
                values.append(_convert_scalar(data, p.ann))
            except ValueError as e:
                errors.append(_err(["body", p.alias], e.args[0], e.args[1]))
                return
        kwargs[p.name] = values if p.many else values[0]

    def _bind_body(self, p, req, kwargs, errors):
        required = p is None or p.required
        if not req.body.strip():
            if required:
                errors.append(_err(["body"], "Field required", "missing"))
            return
        try:
            if p is not None and p.model is not None:
                try:
                    value = p.model.model_validate_json(req.body)
                except Exception as e:  # pydantic.ValidationError を import せずに判定
                    if _is_validation_error(e):
                        errors.extend(_model_errors(e, ["body"]))
                        return
                    raise
            else:
                try:
                    value = json.loads(req.body)
                except ValueError as e:
                    errors.append(_err(["body"], "JSON decode error: %s" % e, "json_invalid"))
                    return
                if self.body_constraints:
                    errs = validate_constraints(value, self.body_constraints)
                    if errs:
                        errors.extend(errs)
                        return
        except UnicodeDecodeError:
            errors.append(_err(["body"], "JSON decode error", "json_invalid"))
            return
        if p is not None:
            kwargs[p.name] = value

    # ---- 戻り値変換 ----

    def to_response(self, result):
        """Convert a handler return value (dict, str, tuple, ``None``, ...) to a :class:`Response`."""
        status = self.status_code
        extra = None
        if isinstance(result, Response):
            return result
        stream = _stream_of(result, status)
        if stream is not None:
            return stream
        if isinstance(result, tuple) and len(result) in (2, 3):
            if len(result) == 3:
                result, status, extra = result
            else:
                result, status = result
            if isinstance(result, Response):
                return result
        if self.response_class is not None and result is not None:
            r = self.response_class(result, status)
        elif result is None:
            r = Response(b"", 204 if status == 200 else status)
        elif isinstance(result, (bytes, bytearray)):
            r = Response(bytes(result), status, media_type="application/octet-stream")
        elif isinstance(result, str):
            r = Response(result, status, media_type="text/plain; charset=utf-8")
        elif hasattr(result, "model_dump_json"):
            r = Response(result.model_dump_json(), status, media_type=_JSON_CT)
        else:
            r = JSONResponse(result, status)
        if extra:
            r.headers.extend(Response(b"", headers=extra).headers)
        return r

    def handle(self, req):
        """Run the handler for a request and return a :class:`Response` (``HTTPException`` and validation errors become responses here)."""
        try:
            result = self._invoke(req)
            if inspect.iscoroutine(result):
                result = _run_coro(result)
            return self.to_response(result)
        except _ValidationFailed as e:
            return JSONResponse({"detail": e.errors}, 422)
        except HTTPException as e:
            return JSONResponse({"detail": e.detail}, e.status_code, e.headers)

    # ---- サーバからの呼び出し（F-212） ----

    def _make_call(self):
        """Pick, ahead of time and from the kinds of parameters, the function the server (Rust) calls per request.

        It returns the raw handler result (``str``, ``dict``, ``Response`` and so on); Rust turns it into a response and
        passes unsupported types to ``to_response``. Validation errors and ``HTTPException`` give the same response as ``handle``.
        """
        func = self.func
        is_async = self.is_async
        positional = (inspect.Parameter.POSITIONAL_ONLY, inspect.Parameter.POSITIONAL_OR_KEYWORD)
        sig = inspect.signature(func).parameters
        if self.uses_di:
            # F-268: Depends 等を使うルート（依存の木を決まった順に呼ぶ）
            if is_async:
                def invoke(req):
                    return self._di_async(req)
            else:
                def invoke(req):
                    return self._di_sync(req)
        elif not self.params and not self.body_constraints:
            def invoke(req):
                return func()
        elif (
            len(self.params) == 1
            and self.params[0].kind == "request"
            and not self.body_constraints
            and sig[self.params[0].name].kind in positional
        ):
            def invoke(req):
                return func(req)
        elif (
            self.params
            and not self.body_constraints
            and all(
                p.kind == "path" and p.ann in (str, _EMPTY) and sig[p.name].kind in positional
                for p in self.params
            )
        ):
            # パスの引数だけ（型変換無しの str）: ルートが当たればパスの引数は必ずある
            names = tuple(p.name for p in self.params)
            if len(names) == 1:
                (n,) = names

                def invoke(req):
                    return func(req.params[n])
            else:
                def invoke(req):
                    params = req.params
                    return func(*[params[n] for n in names])
        else:
            bind = self.bind

            def invoke(req):
                return func(**bind(req))

        def call(req):
            try:
                result = invoke(req)
                if is_async:
                    result = _run_coro(result)
                return result
            except _ValidationFailed as e:
                return JSONResponse({"detail": e.errors}, 422)
            except HTTPException as e:
                return JSONResponse({"detail": e.detail}, e.status_code, e.headers)

        self._invoke = invoke
        return call

    # ---- 依存（F-268） ----

    def _di_sync(self, req):
        ctx = _deps.Ctx()
        try:
            kwargs = _deps.solve_sync(self._tree(), req, ctx, self._bind_one)
            if self.body_constraints and not ctx.state.get("body_checked") and not any(
                    p.kind == "body" for p in self.params):
                self._bind_body(None, req, kwargs, ctx.errors)
            if ctx.errors:
                raise _ValidationFailed(ctx.errors)
            result = self.func(**kwargs)
        except BaseException as e:
            raise _deps.finish_sync(ctx, e)
        exc = _deps.finish_sync(ctx, None)
        if exc is not None:
            raise exc
        return self._shape(result, ctx)

    async def _di_async(self, req):
        ctx = _deps.Ctx()
        try:
            kwargs = await _deps.solve_async(self._tree(), req, ctx, self._bind_one)
            if self.body_constraints and not ctx.state.get("body_checked") and not any(
                    p.kind == "body" for p in self.params):
                self._bind_body(None, req, kwargs, ctx.errors)
            if ctx.errors:
                raise _ValidationFailed(ctx.errors)
            result = await self.func(**kwargs)
        except BaseException as e:
            raise await _deps.finish_async(ctx, e)
        exc = await _deps.finish_async(ctx, None)
        if exc is not None:
            raise exc
        return self._shape(result, ctx)

    def _shape(self, result, ctx):
        """Apply ``response_model``, the ``Response`` parameter (status, headers) and the background tasks."""
        sub = ctx.response
        bg = ctx.background
        if self.response_model is None and self.response_class is None and sub is None and not bg:
            return result
        if isinstance(result, Response):
            r = result
        else:
            if self.response_model is not None:
                result = self._dump_model(result)
            r = self.to_response(result)
            if sub is not None and sub.status:
                r.status = sub.status
        if sub is not None and sub.headers:
            r.headers = list(r.headers) + sub.headers
        if bg:
            prev = r.background
            if prev is None:
                r.background = bg
            elif isinstance(prev, BackgroundTasks):
                prev.tasks.extend(bg.tasks)
            else:
                bg.tasks.insert(0, (prev, (), {}))
                r.background = bg
        return r

    def _dump_model(self, result):
        # response_model: pydantic で検証・絞り込みして JSON にする（pydantic が無ければそのまま）
        ad = self._adapter
        if ad is None:
            try:
                from pydantic import TypeAdapter
            except ImportError:
                return result
            ad = self._adapter = TypeAdapter(self.response_model)
        if hasattr(result, "model_dump") and _is_model(self.response_model) and not isinstance(
                result, self.response_model):
            result = result.model_dump()
        value = ad.validate_python(result, from_attributes=True)
        body = ad.dump_json(value, exclude_none=self.response_model_exclude_none)
        return Response(body, self.status_code, media_type=_JSON_CT)

    async def _acall(self, req):
        """Like ``_call``, on the persistent event loop (the coroutine is awaited instead of run to completion)."""
        try:
            result = self._invoke(req)
            if inspect.isawaitable(result):
                result = await result
            return result
        except _ValidationFailed as e:
            return JSONResponse({"detail": e.errors}, 422)
        except HTTPException as e:
            return JSONResponse({"detail": e.detail}, e.status_code, e.headers)

    # ---- OpenAPI ----

    def openapi(self):
        """Return the OpenAPI Operation Object (a dict) of this route."""
        func = self.func
        doc = inspect.getdoc(func) or ""
        lines = doc.strip().splitlines()
        summary = self.summary or (lines[0].strip() if lines else func.__name__.replace("_", " ").title())
        op = {"operationId": self.operation_id or func.__name__, "summary": summary}
        if self.description:
            op["description"] = self.description
        elif self.summary and doc:
            op["description"] = doc
        elif len(lines) > 1:
            op["description"] = "\n".join(lines[1:]).strip()
        if self.tags:
            op["tags"] = self.tags
        if self.deprecated:
            op["deprecated"] = True
        comps = {}
        flat, security, schemes = self._flat_params()
        params = []
        for p in flat:
            if p.kind not in ("path", "query", "header", "cookie"):
                continue
            m = p.marker
            if m is not None and not m.include_in_schema:
                continue
            schema = _scalar_schema(p.ann)
            if p.rules:
                schema.update(p.rules)
            if p.many:
                schema = {"type": "array", "items": schema}
            if p.default is not _EMPTY and p.default is not None and not isinstance(p.default, (dict, set)):
                schema["default"] = list(p.default) if isinstance(p.default, tuple) else p.default
            one = {"name": p.alias, "in": p.kind, "required": p.kind == "path" or p.required, "schema": schema}
            if m is not None:
                if m.description:
                    one["description"] = m.description
                if m.deprecated:
                    one["deprecated"] = True
                if m.examples:
                    schema["examples"] = list(m.examples)
            params.append(one)
        if params:
            op["parameters"] = params
        bodies = [p for p in flat if p.kind == "body"]
        embedded = len(bodies) > 1 or any(p.marker is not None and p.marker.embed for p in bodies)
        body = bodies[0] if bodies and not embedded else None
        schema = None
        required = True
        form = [p for p in flat if p.kind in ("form", "file")]
        if form:
            # F-266: ファイルがあれば multipart/form-data、無ければ urlencoded
            props = {}
            for p in form:
                if p.kind == "file":
                    one = {"type": "string", "format": "binary"}
                else:
                    one = _scalar_schema(p.ann)
                    if p.default is not _EMPTY and p.default is not None:
                        one["default"] = p.default
                props[p.alias] = {"type": "array", "items": one} if p.many else one
            fs = {"type": "object", "properties": props}
            req_names = [p.alias for p in form if p.required]
            if req_names:
                fs["required"] = req_names
            media = "multipart/form-data" if any(p.kind == "file" for p in form) else "application/x-www-form-urlencoded"
            op["requestBody"] = {"required": bool(req_names), "content": {media: {"schema": fs}}}
        if embedded:
            props = {}
            req_names = []
            for p in bodies:
                if p.model is not None:
                    props[p.alias] = self._model_ref(p.model, comps)
                else:
                    one = _scalar_schema(p.ann) if not _is_container(p.ann) else {"type": "object"}
                    if p.rules:
                        one.update(p.rules)
                    props[p.alias] = one
                if p.required:
                    req_names.append(p.alias)
            schema = {"type": "object", "properties": props}
            if req_names:
                schema["required"] = req_names
            required = bool(req_names)
        elif body is not None and body.model is not None:
            schema = self._model_ref(body.model, comps)
            required = body.required
        elif self.body_constraints:
            schema = constraints_schema(self.body_constraints)
        elif body is not None:
            schema = {"type": "array"} if (body.ann is list or typing.get_origin(body.ann) is list) else {"type": "object"}
            required = body.required
        if schema is not None:
            media = _JSON_CT
            if body is not None and body.marker is not None:
                media = body.marker.media_type
            op["requestBody"] = {"required": required, "content": {media: {"schema": schema}}}
        ok = {"description": "Successful Response"}
        ret = self.response_model if self.response_model is not None else self._hints.get("return")
        rs = self._type_schema(ret, comps) if ret is not None else None
        if self.response_class is not None and self.response_class is not JSONResponse:
            media = {HTMLResponse: "text/html", PlainTextResponse: "text/plain"}.get(self.response_class)
            ok["content"] = {media: {"schema": {"type": "string"}}} if media else {}
            if not ok["content"]:
                del ok["content"]
        else:
            ok["content"] = {_JSON_CT: {"schema": rs if rs is not None else {}}}
        responses = {str(self.status_code): ok}
        if params or schema is not None or form:
            responses["422"] = {
                "description": "Validation Error",
                "content": {_JSON_CT: {"schema": {"$ref": "#/components/schemas/HTTPValidationError"}}},
            }
        for code, extra in self.responses.items():
            one = dict(responses.get(str(code), {}))
            extra = dict(extra or {})
            model = extra.pop("model", None)
            one.update(extra)
            one.setdefault("description", _status_phrase(code))
            if model is not None:
                one["content"] = {_JSON_CT: {"schema": self._type_schema(model, comps) or {}}}
            responses[str(code)] = one
        op["responses"] = responses
        if security:
            op["security"] = security
            op["x-veil-security-schemes"] = schemes
        if comps:
            op["x-veil-components"] = comps
        return op

    def _flat_params(self):
        """The request parameters of the route and its dependencies (each once), the security requirements and the
        security schemes."""
        seen = set()
        out = []
        security = []
        schemes = {}

        def walk(dep):
            scheme = getattr(dep.call, "openapi_scheme", None)
            if callable(scheme) and not inspect.isclass(dep.call):
                name, model = scheme()
                schemes[name] = model
                req = {name: list(dep.scopes)}
                if req not in security:
                    security.append(req)
            for p in dep.params:
                if p.kind == "depends":
                    walk(p.sub)
                elif p.kind in ("path", "query", "header", "cookie", "body", "form", "file"):
                    key = (p.kind, p.alias)
                    if key not in seen:
                        seen.add(key)
                        out.append(p)

        walk(self._tree())
        return out, security, schemes

    def _type_schema(self, t, comps):
        # 応答の型（pydantic のモデル、list[モデル]、他の型は pydantic の TypeAdapter があればそのスキーマ）
        if _is_model(t):
            return self._model_ref(t, comps)
        if typing.get_origin(t) is list and typing.get_args(t) and _is_model(typing.get_args(t)[0]):
            return {"type": "array", "items": self._model_ref(typing.get_args(t)[0], comps)}
        return None

    @staticmethod
    def _model_ref(model, comps):
        schema = model.model_json_schema(ref_template="#/components/schemas/{model}")
        defs = schema.pop("$defs", {})
        comps.update(defs)
        comps[model.__name__] = schema
        return {"$ref": "#/components/schemas/" + model.__name__}

    def openapi_json(self):
        """Return the Operation Object as a JSON string."""
        return json.dumps(self.openapi(), separators=(",", ":"), ensure_ascii=False)


class WebSocketRoute(Route):
    """A WebSocket route (``@app.websocket``). The handler gets a :class:`WebSocket`; path and query parameters
    are bound as for HTTP routes."""

    def __init__(
        self,
        func,
        path,
        receive=None,
        send=None,
        subprotocols=None,
        tags=None,
        summary=None,
        include_in_schema=True,
        validate=False,
        dependencies=None,
        overrides=None,
    ):
        self.endpoint = func if inspect.isclass(func) and issubclass(func, WebSocketEndpoint) else None
        target = func if self.endpoint is None else (lambda websocket: None)
        super().__init__(target, path, ["GET"], True, None, tags, summary, 101, dependencies=dependencies,
                         overrides=overrides)
        if self.endpoint is not None:
            self.func = func
        self.receive = receive
        self.send = send
        self.subprotocols = list(subprotocols or [])
        self.include_in_schema = include_in_schema
        self.validate = validate
        if self.endpoint is None and not self.is_async:
            raise TypeError("@app.websocket needs an async def handler or a WebSocketEndpoint subclass")
        if any(p.kind == "body" for p in self.params):
            raise TypeError("WebSocket handlers cannot take a request body")

    async def call(self, ws, req):
        """Bind the parameters and run the handler (called on the event loop)."""
        if self.uses_di:
            await self._call_di(ws, req)
            return
        try:
            kwargs = self.bind(req)
        except _ValidationFailed:
            ws._n.reject(422)
            ws.application_state = WebSocketState.DISCONNECTED
            return
        for p in self.params:
            if p.kind == "websocket":
                kwargs[p.name] = ws
        await self.func(**kwargs)

    async def _call_di(self, ws, req):
        # F-268: Depends を使う WebSocket のハンドラ（受け入れ前の HTTPException は HTTP の応答で断る）
        ctx = _deps.Ctx()
        ctx.state["websocket"] = ws
        try:
            try:
                kwargs = await _deps.solve_async(self._tree(), req, ctx, self._bind_one)
                if ctx.errors:
                    raise _ValidationFailed(ctx.errors)
                await self.func(**kwargs)
            except BaseException as e:
                raise await _deps.finish_async(ctx, e)
            exc = await _deps.finish_async(ctx, None)
            if exc is not None:
                raise exc
        except (_ValidationFailed, HTTPException) as e:
            status = 422 if isinstance(e, _ValidationFailed) else e.status_code
            if ws.application_state == WebSocketState.CONNECTING:
                ws._n.reject(status)
                ws.application_state = WebSocketState.DISCONNECTED
            elif ws.application_state != WebSocketState.DISCONNECTED:
                await ws.close(1008)

    def _endpoint_session(self, sender, req):
        # WebSocketEndpoint の接続ごとのセッション（Rust のハンドラ用プールから呼ぶ）
        from .websockets import _EndpointSession

        return _EndpointSession(self.endpoint, sender, req)

    def openapi(self):
        """The Operation Object of the handshake (``GET`` → 101) with ``x-veil-websocket``."""
        func = self.func
        doc = inspect.getdoc(func) or ""
        lines = doc.strip().splitlines()
        name = func.__name__
        summary = self.summary or (lines[0].strip() if lines else name.replace("_", " ").title())
        op = {"operationId": name, "summary": summary}
        if len(lines) > 1:
            op["description"] = "\n".join(lines[1:]).strip()
        if self.tags:
            op["tags"] = self.tags
        params = []
        for p in self.params:
            if p.kind in ("path", "query"):
                schema = _scalar_schema(p.ann)
                if p.default is not _EMPTY and p.default is not None and not isinstance(p.default, (dict, list)):
                    schema["default"] = p.default
                params.append(
                    {"name": p.name, "in": p.kind, "required": p.kind == "path" or p.required, "schema": schema}
                )
        if self.subprotocols:
            params.append(
                {
                    "name": "Sec-WebSocket-Protocol",
                    "in": "header",
                    "required": False,
                    "schema": {"type": "string", "enum": self.subprotocols},
                }
            )
        if params:
            op["parameters"] = params
        comps = {}
        x = {}
        if self.subprotocols:
            x["subprotocols"] = self.subprotocols
        if self.receive is not None:
            x["receive"] = self._model_ref(self.receive, comps)
        if self.send is not None:
            x["send"] = self._model_ref(self.send, comps)
        if self.receive is not None or self.send is not None:
            x["messageFormat"] = "text/json"
        # 応答（101・400・426）・maxMessageSize・接続の例は Rust の OpenAPI の生成が足す
        op["x-veil-websocket"] = x
        if comps:
            op["x-veil-components"] = comps
        return op


class State:
    """Free-form attributes shared by the application (``app.state``; the lifespan may fill it)."""

    def __init__(self, values=None):
        self.__dict__.update(values or {})


# --------------------------------------------------------------------------
# App
# --------------------------------------------------------------------------

_HANDLERS = []  # handler_id → Route（プロセス全体）


def _run_background(tasks):
    """Run background tasks in order (on the handler pool, after the response was sent)."""
    import traceback

    from . import _aio

    for f, args, kwargs in tasks:
        try:
            r = f(*args, **kwargs)
            if inspect.isawaitable(r):
                el = _aio.first()
                if el is not None and threading.current_thread() is not el.thread:
                    el.run_sync(r)
                else:
                    _run_coro(r)
        except Exception:  # noqa: BLE001  1 つの失敗で残りを止めない
            traceback.print_exc()


async def _arun_background(bg):
    """Run ``background`` (a :class:`BackgroundTasks` or any callable) on the event loop."""
    import traceback

    tasks = getattr(bg, "tasks", None)
    if tasks is None:
        tasks = [(bg, (), {})]
    for f, args, kwargs in tasks:
        try:
            if inspect.iscoroutinefunction(f):
                await f(*args, **kwargs)
            else:
                r = await run_in_threadpool(f, *args, **kwargs)
                if inspect.isawaitable(r):
                    await r
        except Exception:  # noqa: BLE001
            traceback.print_exc()


def _dispatch(handler_id, method, path, query, headers, body, params, client_ip):
    """Called from Rust with the GIL held; returns ``(status, headers, body)``."""
    route = _HANDLERS[handler_id]
    req = Request(method, path, query, headers, body, params, client_ip)
    return route.handle(req)._triple()


def _toml_literal(v):
    if isinstance(v, bool):
        return "true" if v else "false"
    if isinstance(v, (int, float)):
        return repr(v)
    if isinstance(v, str):
        return json.dumps(v)
    if isinstance(v, (list, tuple)):
        return "[" + ", ".join(_toml_literal(x) for x in v) + "]"
    if isinstance(v, dict):
        return "{" + ", ".join("%s = %s" % (json.dumps(str(k)), _toml_literal(x)) for k, x in v.items()) + "}"
    raise TypeError("cannot convert to TOML: %r" % (v,))


# F-260: 待ち受け 1 個（``[[listener]]``）。``run(listeners=[...])`` に渡す（辞書でも同じキー）。
_LISTENER_KEYS = (
    "name",
    "http2",
    "http3",
    "http3_listen",
    "allow_http",
    "allow_h2c",
    "secure_redirect",
    "redirect_to",
    "threads",
    "unix_socket_permissions",
)


class Listener:
    """One listening port (a ``[[listener]]`` entry) for ``run(listeners=[...])``.

    ``listen`` is ``host:port`` or ``unix:<path>``. ``tls=True`` serves TLS with the ``tls_cert`` / ``tls_key``
    certificate; ``http2`` (TLS only, default ``[server].http2_enabled``) offers h2 through ALPN and ``http3``
    (TLS only) also serves HTTP/3 on the same UDP port (or ``http3_listen``). Plaintext listeners accept
    HTTP/1.1 (``allow_http``) and h2c (``allow_h2c``), both on by default; ``secure_redirect=True`` answers
    with a 301 redirect to https (to the port of the TLS listener ``redirect_to``, default the first one).
    ``threads`` is the number of worker threads of this listener (default ``[server].threads``).
    """

    def __init__(self, listen, tls=False, **options):
        """Create a listener on ``listen``; ``options`` are the ``[[listener]]`` keys listed above."""
        unknown = set(options) - set(_LISTENER_KEYS)
        if unknown:
            raise TypeError("unknown listener option(s): %s" % ", ".join(sorted(unknown)))
        self.listen = listen
        self.tls = bool(tls)
        self.options = {k: v for k, v in options.items() if v is not None}

    def __repr__(self):
        opts = "".join(", %s=%r" % kv for kv in sorted(self.options.items()))
        return "Listener(%r, tls=%r%s)" % (self.listen, self.tls, opts)

    @classmethod
    def from_value(cls, value):
        """Return ``value`` as a :class:`Listener` (a ``Listener`` or a dict with the same keys)."""
        if isinstance(value, Listener):
            return value
        if isinstance(value, dict):
            d = dict(value)
            if "listen" not in d:
                raise ValueError("a listener needs 'listen'")
            return cls(d.pop("listen"), d.pop("tls", False), **d)
        raise TypeError("a listener must be veil.Listener or a dict, not %r" % (value,))

    def add_to(self, cfg):
        """Add this listener to the native ``Config``."""
        cfg.add_listener(self.listen, self.tls, **self.options)


class App:
    """A set of routes. Register with ``@app.get(...)`` and start with ``app.run()``."""

    def __init__(self, title="veil API", version="0.1.0", lifespan=None, on_startup=None, on_shutdown=None,
                 dependencies=None):
        """Create an application with the OpenAPI ``title`` and ``version``.

        ``dependencies`` (``Depends(...)`` values) run for every route, before the route's own.

        ``lifespan`` is FastAPI's: a function taking the app and returning an async context manager (for example
        an ``@contextlib.asynccontextmanager`` generator). Its code before ``yield`` runs on the event loop before
        the listeners open (an exception stops the start with exit code 1), the code after ``yield`` once the
        server stopped. A dict it yields is merged into ``app.state``.
        """
        self.title = title
        self.version = version
        self.lifespan = lifespan
        self.startup_handlers = list(on_startup or [])
        self.shutdown_handlers = list(on_shutdown or [])
        self.state = State()
        self.websocket_routes = []
        self.routes = []
        self.grpc_methods = []  # (path, handler)  handler.handle(call) -> (code, message)
        self.grpc_health = False
        self.l4 = []  # ("tcp" | "udp", name, listen, factory_or_func)
        # F-268: 依存の差し替え（試験用。変えると次の要求から依存の木を作り直す）・アプリ全体の依存・
        # ASGI のアプリ・CORS・静的ファイル
        self.dependency_overrides = _deps.Overrides()
        self.dependencies = list(dependencies or [])
        self.asgi_mounts = []
        self.cors_options = []
        self.static_dirs = []

    def route(
        self,
        path,
        methods=None,
        offload=True,
        body_constraints=None,
        tags=None,
        summary=None,
        status_code=200,
        stream_body=False,
        async_mode=None,
        compress=True,
        *,
        dependencies=None,
        response_model=None,
        responses=None,
        include_in_schema=True,
        response_class=None,
        deprecated=None,
        description=None,
        operation_id=None,
        name=None,
        response_model_exclude_none=False,
    ):
        """Register a handler for ``path`` and ``methods`` (default ``["GET"]``); use as a decorator.

        ``offload=True`` runs the handler on the handler pool, ``False`` on the network worker (trivially cheap
        handlers only). ``body_constraints`` validates dict bodies without pydantic. ``tags`` and ``summary`` go
        to OpenAPI, ``status_code`` is the default status. With ``stream_body=True`` the handler is called before
        the request body has arrived and ``req.stream()`` reads it as it comes.

        ``async def`` handlers run on the persistent event loop (like uvicorn: do not block in them, use
        :func:`run_in_threadpool`). ``async_mode="pool"`` runs them to completion on the handler pool instead, as
        before (one pool thread per waiting request); ``stream_body=True`` routes always do.

        ``compress=False`` keeps the server from compressing this route's responses (``[route.compression]`` and
        ``[api] compression``, see :meth:`run`).

        FastAPI's arguments: ``dependencies`` (``Depends(...)`` run before the handler, results not passed),
        ``response_model`` (validates and filters the return value with pydantic), ``responses`` (more OpenAPI
        responses: ``{404: {"model": Error, "description": ...}}``), ``include_in_schema``, ``response_class``
        (e.g. :class:`HTMLResponse`), ``deprecated``, ``description`` and ``operation_id``.
        """

        def deco(func):
            self.routes.append(
                Route(
                    func,
                    path,
                    methods or ["GET"],
                    offload,
                    body_constraints,
                    tags,
                    summary,
                    status_code,
                    stream_body,
                    async_mode,
                    compress,
                    dependencies=self.dependencies + list(dependencies or []),
                    response_model=response_model,
                    responses=responses,
                    include_in_schema=include_in_schema,
                    response_class=response_class,
                    deprecated=deprecated,
                    description=description,
                    operation_id=operation_id,
                    overrides=self.dependency_overrides,
                    response_model_exclude_none=response_model_exclude_none,
                )
            )
            return func

        return deco

    def websocket(
        self,
        path,
        *,
        receive=None,
        send=None,
        subprotocols=None,
        tags=None,
        summary=None,
        include_in_schema=True,
        validate=False,
        dependencies=None,
        name=None,
    ):
        """Register a WebSocket handler for ``path`` (HTTP/1.1); use as a decorator.

        Decorate an ``async def handler(websocket: veil.WebSocket, ...)`` (it runs on the event loop and must call
        ``await websocket.accept()``) or a :class:`WebSocketEndpoint` subclass. ``receive`` / ``send`` are pydantic
        models of the JSON messages for OpenAPI (``x-veil-websocket``); with ``validate=True``
        ``websocket.receive_json()`` closes with 1007 when a message does not match ``receive``. ``subprotocols``
        lists the ``Sec-WebSocket-Protocol`` values for OpenAPI. ``include_in_schema=False`` leaves the route out
        of OpenAPI.
        """

        def deco(func):
            self.websocket_routes.append(
                WebSocketRoute(
                    func, path, receive, send, subprotocols, tags, summary, include_in_schema, validate,
                    self.dependencies + list(dependencies or []), self.dependency_overrides,
                )
            )
            return func

        return deco

    # ---- 起動と終了（lifespan） ----

    def on_startup(self, func):
        """Register a function (``def`` or ``async def``) run before the server starts; use as a decorator."""
        self.startup_handlers.append(func)
        return func

    def on_shutdown(self, func):
        """Register a function (``def`` or ``async def``) run after the server stopped; use as a decorator."""
        self.shutdown_handlers.append(func)
        return func

    def on_event(self, event):
        """FastAPI's ``@app.on_event("startup" | "shutdown")``."""
        if event == "startup":
            return self.on_startup
        if event == "shutdown":
            return self.on_shutdown
        raise ValueError('event must be "startup" or "shutdown"')

    async def _startup(self):
        if self.lifespan is not None:
            self._lifespan_cm = self.lifespan(self)
            v = await self._lifespan_cm.__aenter__()
            if isinstance(v, dict):
                self.state.__dict__.update(v)
        for f in self.startup_handlers:
            r = f()
            if inspect.isawaitable(r):
                await r
        for m in self.asgi_mounts:
            await m.startup()

    async def _shutdown(self):
        for m in self.asgi_mounts:
            try:
                await m.shutdown()
            except Exception:  # noqa: BLE001
                import traceback

                traceback.print_exc()
        for f in self.shutdown_handlers:
            try:
                r = f()
                if inspect.isawaitable(r):
                    await r
            except Exception:  # noqa: BLE001  残りの終了処理も行う
                import traceback

                traceback.print_exc()
        cm = getattr(self, "_lifespan_cm", None)
        if cm is not None:
            self._lifespan_cm = None
            await cm.__aexit__(None, None, None)

    # ---- L4（F-210） ----

    def l4_tcp(self, name, listen):
        """Open the TCP listener ``listen`` and handle each connection with ``factory()`` (e.g. a :class:`Protocol` subclass); use as a decorator."""

        def deco(factory):
            self.l4.append(("tcp", name, listen, factory))
            return factory

        return deco

    def l4_udp(self, name, listen):
        """Open the UDP listener ``listen`` and call ``func(data, peer)`` per datagram (``peer`` is ``(host, port)``); a returned ``bytes`` is sent back; use as a decorator."""

        def deco(func):
            self.l4.append(("udp", name, listen, func))
            return func

        return deco

    def _verb(self, verb):
        def deco(path, **kw):
            kw.pop("methods", None)
            return self.route(path, methods=[verb], **kw)

        return deco

    def get(self, path, **kw):
        """Decorator registering a ``GET`` handler (same keyword arguments as :meth:`route`)."""
        return self._verb("GET")(path, **kw)

    def post(self, path, **kw):
        """Decorator registering a ``POST`` handler (same keyword arguments as :meth:`route`)."""
        return self._verb("POST")(path, **kw)

    def put(self, path, **kw):
        """Decorator registering a ``PUT`` handler (same keyword arguments as :meth:`route`)."""
        return self._verb("PUT")(path, **kw)

    def delete(self, path, **kw):
        """Decorator registering a ``DELETE`` handler (same keyword arguments as :meth:`route`)."""
        return self._verb("DELETE")(path, **kw)

    def patch(self, path, **kw):
        """Decorator registering a ``PATCH`` handler (same keyword arguments as :meth:`route`)."""
        return self._verb("PATCH")(path, **kw)

    def head(self, path, **kw):
        """Decorator registering a ``HEAD`` handler (same keyword arguments as :meth:`route`)."""
        return self._verb("HEAD")(path, **kw)

    def options(self, path, **kw):
        """Decorator registering an ``OPTIONS`` handler (same keyword arguments as :meth:`route`)."""
        return self._verb("OPTIONS")(path, **kw)

    # ---- FastAPI の構成（F-268） ----

    def include_router(self, router, prefix="", tags=None, dependencies=None, responses=None):
        """Add the routes of an :class:`APIRouter` (under ``prefix``, with extra ``tags`` / ``dependencies``)."""
        if prefix and (not prefix.startswith("/") or prefix.endswith("/")):
            raise ValueError("a prefix must start with '/' and not end with '/'")
        for kind, path, kw, func in router._expanded(prefix, tags, dependencies, responses):
            if kind == "route":
                self.route(path, **kw)(func)
            else:
                self.websocket(path, **kw)(func)

    def add_middleware(self, middleware_class, *args, **options):
        """Install a middleware that veil implements natively (``veil_kit.cors.CORSMiddleware``).

        ASGI middlewares of Starlette are not run around native routes; mount an ASGI application (``mount``) to
        use them.
        """
        install = getattr(middleware_class, "_veil_install", None)
        if install is None:
            raise TypeError(
                "%s is not supported by veil (supported: veil_kit.cors.CORSMiddleware; ASGI middleware works inside "
                "an ASGI application mounted with app.mount)" % getattr(middleware_class, "__name__", middleware_class)
            )
        install(self, *args, **options)

    def cors(self, allow_origins=(), allow_methods=("GET",), allow_headers=(), allow_credentials=False,
             expose_headers=(), max_age=600, prefix="/"):
        """Answer CORS in the server (preflight requests do not reach Python; the headers are built at startup).

        The arguments are those of Starlette's ``CORSMiddleware``: ``"*"`` in ``allow_origins`` allows any origin
        (not with ``allow_credentials``), ``https://*.example.com`` allows subdomains, ``["*"]`` in
        ``allow_methods`` / ``allow_headers`` allows all. ``prefix`` limits CORS to the routes below a path.
        """
        origins = [o for o in allow_origins if o != "*"]
        methods = list(allow_methods)
        if "*" in methods:
            methods = ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"]
        headers = [h for h in allow_headers if h != "*"]
        self.cors_options.append({
            "prefix": prefix,
            "origins": origins,
            "any_origin": "*" in allow_origins,
            "methods": methods,
            "headers": headers,
            "any_header": "*" in allow_headers,
            "expose": list(expose_headers),
            "credentials": bool(allow_credentials),
            "max_age": None if max_age is None else int(max_age),
        })

    def serve_dir(self, path, directory, index=None, cache=True, **route_options):
        """Serve ``directory`` below ``path`` with the server's file backend (sendfile, Range, ``ETag``; requests do
        not reach Python). ``index`` is the file served for a directory; ``cache=False`` turns off the open-file and
        content caches; ``route_options`` are more ``[[route]]`` keys (e.g. ``security={...}``)."""
        import os

        extra = "\n".join("%s = %s" % (k, _toml_literal(v)) for k, v in route_options.items()) or None
        self.static_dirs.append((path.rstrip("/") or "/", os.path.abspath(directory), index, bool(cache), extra))

    def mount(self, path, app, name=None):
        """Mount ``app`` at ``path``: a ``veil_kit.staticfiles.StaticFiles`` or an ASGI application (FastAPI,
        Starlette, ...; see :mod:`veil.asgi`)."""
        prefix = path.rstrip("/")
        hook = getattr(app, "_veil_mount", None)
        if hook is not None:
            hook(self, prefix or "/")
            return
        if callable(app):
            from .asgi import ASGIApp

            self.asgi_mounts.append(ASGIApp(app, prefix))
            return
        raise TypeError("app.mount() takes StaticFiles or an ASGI application, not %r" % (app,))

    def build_config(
        self,
        config_path=None,
        *,
        listen_cleartext=None,
        listen=None,
        tls_cert=None,
        tls_key=None,
        threads=None,
        overrides=None,
        openapi=True,
        listeners=None,
        http3=None,
        openapi_docs_ui=None,
    ):
        """Build the native ``Config`` from the arguments of :meth:`run` (requires the native extension)."""
        from . import _veil

        cfg = _veil.Config.from_file(config_path) if config_path else _veil.Config()
        if self.title != "veil API":
            cfg.set("api.openapi_title", _toml_literal(self.title))
        if self.version != "0.1.0":
            cfg.set("api.openapi_version", _toml_literal(self.version))
        if not openapi:
            cfg.set("api.openapi_enabled", "false")
        if openapi_docs_ui is not None:
            # "embedded"（同梱の Swagger UI。既定）・"cdn"・"off"（F-264）
            cfg.set("api.openapi_docs_ui", _toml_literal(openapi_docs_ui))
        if listen_cleartext:
            cfg.listen_cleartext(listen_cleartext)
        if listen:
            cfg.listen(listen)
        for listener in listeners or ():
            Listener.from_value(listener).add_to(cfg)
        if http3:
            # [server].listen の TLS のリスナーで HTTP/3 も受ける（文字列なら UDP のアドレス）
            cfg.set("http3.enabled", "true")
            if isinstance(http3, str):
                cfg.set("http3.listen", _toml_literal(http3))
        if tls_cert or tls_key:
            if not (tls_cert and tls_key):
                raise ValueError("tls_cert and tls_key must be given together")
            cfg.tls(str(tls_cert), str(tls_key))
        if threads:
            cfg.threads(int(threads))
        for k, v in (overrides or {}).items():
            cfg.set(k, _toml_literal(v))
        return cfg

    def run(
        self,
        config_path=None,
        *,
        listen_cleartext=None,
        listen=None,
        tls_cert=None,
        tls_key=None,
        threads=None,
        overrides=None,
        openapi=True,
        listeners=None,
        http3=None,
        event_loops=None,
        event_loop=None,
        async_mode=None,
        compression=None,
        openapi_docs_ui=None,
    ):
        """Start the server and block until it stops (Ctrl-C, SIGTERM or :func:`shutdown`).

        ``listen_cleartext`` is the plaintext HTTP/1.1 + h2c address (default ``127.0.0.1:8080``); ``listen`` is
        the TLS address and needs ``tls_cert`` and ``tls_key``. ``http3=True`` also serves HTTP/3 on the UDP port
        of ``listen`` (a string is the UDP address). ``listeners`` is a list of :class:`Listener` (or dicts with
        the same keys) for several ports; with any listener the default ``127.0.0.1:8080`` is not opened unless
        ``listen_cleartext`` is given. ``overrides`` maps config keys to Python values, e.g.
        ``{"server.threads": 4}``. ``openapi=False`` disables ``/openapi.json`` and ``/docs``.

        ``event_loops`` (``[api] python_event_loops``, default 1) is the number of persistent event loops,
        ``event_loop`` (``[api] python_event_loop``: ``"auto"``, ``"asyncio"`` or ``"uvloop"``) their kind, and
        ``async_mode`` (``[api] python_async_mode``: ``"loop"`` or ``"pool"``) where ``async def`` handlers run
        by default.

        ``compression=True`` (``[api] compression``) compresses handler responses with gzip or brotli when the
        client accepts it (``Accept-Encoding``), for routes without their own ``[route.compression]``.

        ``openapi_docs_ui`` chooses how ``/docs`` serves Swagger UI: ``"embedded"`` (the default; bundled assets,
        no network access), ``"cdn"`` or ``"off"``.
        """
        from . import _aio, _veil

        if compression is not None:
            overrides = dict(overrides or {})
            overrides["api.compression"] = bool(compression)

        cfg = self.build_config(
            config_path,
            listen_cleartext=listen_cleartext,
            listen=listen,
            tls_cert=tls_cert,
            tls_key=tls_key,
            threads=threads,
            overrides=overrides,
            openapi=openapi,
            listeners=listeners,
            http3=http3,
            openapi_docs_ui=openapi_docs_ui,
        )
        # サーバの中で渡る Rust 側の型も isinstance(x, veil.Request / veil.Headers) で真にする
        Request.register(_veil.NativeRequest)
        Headers.register(_veil.NativeHeaders)

        def setting(arg, key, default):
            if arg is not None:
                return arg
            v = cfg.api_setting(key)
            return default if v is None else v

        mode = setting(async_mode, "python_async_mode", "loop")
        if mode not in ("loop", "pool"):
            raise ValueError('python_async_mode must be "loop" or "pool", not %r' % (mode,))
        for r in self.routes:
            hid = len(_HANDLERS)
            _HANDLERS.append(r)
            on_loop = r.is_async and not r.stream_body and (r.async_mode or mode) == "loop"
            _veil.register_route(
                r.path,
                r.methods,
                hid,
                r.offload,
                r.openapi_json(),
                r.status_code,
                r.stream_body,
                on_loop,
                any(p.kind in ("form", "file") for p in r._flat_params()[0]),
                r.compress,
                not r.include_in_schema,
            )
        for r in self.websocket_routes:
            hid = len(_HANDLERS)
            _HANDLERS.append(r)
            meta = r.openapi_json() if r.include_in_schema else None
            _veil.register_websocket(r.path, hid, meta, r.endpoint is not None, not r.include_in_schema)
        for path, handler in self.grpc_methods:
            hid = len(_HANDLERS)
            _HANDLERS.append(handler)
            _veil.register_grpc(path, hid)
        if self.grpc_health:
            _veil.register_grpc_health()
        for m in self.asgi_mounts:
            hid = len(_HANDLERS)
            _HANDLERS.append(m)
            _veil.register_asgi(m.prefix, hid)
        for prefix, directory, index, cache, extra in self.static_dirs:
            _veil.register_static(prefix, directory, index, cache, extra)
        for opts in self.cors_options:
            _veil.register_cors(**opts)
        for kind, name, listen, f in self.l4:
            hid = len(_HANDLERS)
            _HANDLERS.append(f)
            (_veil.register_l4_tcp if kind == "tcp" else _veil.register_l4_udp)(name, listen, hid)
        # 常駐のイベントループ（ワーカーは GIL を取らず、キュー + ソケットの組で投入する）
        fds = _aio.start(
            setting(event_loops, "python_event_loops", 1), setting(event_loop, "python_event_loop", "auto")
        )
        _veil.aio_setup(fds, int(setting(None, "python_event_loop_queue", 65536)))
        el = _aio.first()
        try:
            el.run_sync(self._startup())
        except Exception:  # noqa: BLE001  起動を止める（終了コード 1）
            import sys
            import traceback

            traceback.print_exc()
            print("veil: application startup failed; exiting", file=sys.stderr)
            raise SystemExit(1)
        try:
            _veil.run(cfg, _HANDLERS)
        finally:
            try:
                el.run_sync(self._shutdown(), 30)
            finally:
                _aio.stop()


# モジュールレベルの既定 App（``@veil.get(...)`` / ``veil.run(...)``）
_default_app = App()
route = _default_app.route
get = _default_app.get
post = _default_app.post
put = _default_app.put
delete = _default_app.delete
patch = _default_app.patch
run = _default_app.run
websocket = _default_app.websocket
on_startup = _default_app.on_startup
on_shutdown = _default_app.on_shutdown
on_event = _default_app.on_event
l4_tcp = _default_app.l4_tcp
l4_udp = _default_app.l4_udp
include_router = _default_app.include_router
mount = _default_app.mount
add_middleware = _default_app.add_middleware


def shutdown():
    """Stop the running server gracefully."""
    from . import _veil

    _veil.shutdown()


def __getattr__(name):
    # ``veil.Config`` はネイティブ拡張の遅延 import
    if name == "Config":
        from . import _veil

        return _veil.Config
    raise AttributeError(name)
