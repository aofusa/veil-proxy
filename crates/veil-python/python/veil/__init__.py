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
from . import _forms
from ._forms import File, Form, UploadFile
from .websockets import WebSocket, WebSocketDisconnect, WebSocketEndpoint, WebSocketState

__all__ = [
    "App",
    "Request",
    "Response",
    "JSONResponse",
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

    def __init__(self, body=b"", status=200, headers=None, media_type=None, compress=True):
        """Create a response from a body, status, optional headers (dict or pairs) and media type."""
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

    @staticmethod
    def switching_protocols(protocol, factory, headers=None):
        """Answer ``101 Switching Protocols`` to ``protocol`` (HTTP/1.1 only) and serve the connection with ``factory()``.

        ``factory`` returns a :class:`Protocol` (callbacks on the handler pool) or an :class:`AsyncProtocol`
        (``async def`` callbacks on the event loop). The request must ask for the upgrade
        (``Connection: upgrade`` and ``Upgrade``); otherwise veil answers 400.
        """
        return SwitchingProtocols(protocol, factory, headers)


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

    def __init__(self, content, status=200, headers=None, media_type=None, trailers=None, compress=True):
        """Create a streaming response from ``content`` (see the class documentation)."""
        super().__init__(b"", status, headers, media_type, compress)
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

    def __init__(self, content, status=200, headers=None):
        """Create an SSE response from an iterable or generator of events."""
        super().__init__(content, status, headers, "text/event-stream")
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
    def __init__(self, content=None, status=200, headers=None, compress=True):
        """Create a JSON response from a JSON-serialisable ``content``."""
        super().__init__(_dumps(content), status, headers, _JSON_CT, compress)


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
    __slots__ = ("name", "kind", "ann", "model", "default", "required", "alias", "many")

    def __init__(self, name, kind, ann, model, default, alias=None, many=False):
        self.name = name
        self.kind = kind  # request / path / body / query / form / file
        self.ann = ann
        self.model = model
        self.default = default
        self.required = default is _EMPTY
        # F-266: フォームのフィールド名（既定は引数名）と、繰り返しを list で受けるか
        self.alias = alias or name
        self.many = many


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


def _build_params(func, path_names):
    hints = _resolve_hints(func)
    try:
        extras = typing.get_type_hints(func, include_extras=True)
    except Exception:
        extras = {}
    out = []
    for name, p in inspect.signature(func).parameters.items():
        if p.kind in (p.VAR_POSITIONAL, p.VAR_KEYWORD):
            continue
        ann = hints.get(name, p.annotation)
        if isinstance(ann, str):
            ann = _STR_ANN.get(ann, _EMPTY)
        base, optional = _unwrap_optional(ann)
        marker = _form_marker(p, extras.get(name, p.annotation))
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
        if "pattern" in rules and re.fullmatch(rules["pattern"], v) is None:
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
    ):
        self.func = func
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
        self.params = _build_params(func, set(self.path_names))
        self.is_async = inspect.iscoroutinefunction(func)
        self._hints = _resolve_hints(func)
        self._call = self._make_call()

    # ---- 引数束縛 ----

    def bind(self, req):
        """Build the function arguments from the request; validation errors raise an internal exception that becomes a 422 response."""
        kwargs = {}
        errors = []
        qp = None
        body_checked = False
        form = None
        for p in self.params:
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
        if result is None:
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
            kwargs = self.bind(req)
            result = self.func(**kwargs)
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
        if not self.params and not self.body_constraints:
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
        op = {"operationId": func.__name__, "summary": summary}
        if self.summary and doc:
            op["description"] = doc
        elif len(lines) > 1:
            op["description"] = "\n".join(lines[1:]).strip()
        if self.tags:
            op["tags"] = self.tags
        comps = {}
        params = []
        for p in self.params:
            if p.kind in ("path", "query"):
                schema = _scalar_schema(p.ann)
                if p.default is not _EMPTY and p.default is not None and not isinstance(p.default, (dict, list)):
                    schema["default"] = p.default
                params.append(
                    {
                        "name": p.name,
                        "in": p.kind,
                        "required": p.kind == "path" or p.required,
                        "schema": schema,
                    }
                )
        if params:
            op["parameters"] = params
        body = next((p for p in self.params if p.kind == "body"), None)
        schema = None
        required = True
        form = [p for p in self.params if p.kind in ("form", "file")]
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
        if body is not None and body.model is not None:
            schema = self._model_ref(body.model, comps)
            required = body.required
        elif self.body_constraints:
            schema = constraints_schema(self.body_constraints)
        elif body is not None:
            schema = {"type": "array"} if (body.ann is list or typing.get_origin(body.ann) is list) else {"type": "object"}
            required = body.required
        if schema is not None:
            op["requestBody"] = {"required": required, "content": {_JSON_CT: {"schema": schema}}}
        ok = {"description": "Successful Response"}
        ret = self._hints.get("return")
        if ret is not None and _is_model(ret):
            ok["content"] = {_JSON_CT: {"schema": self._model_ref(ret, comps)}}
        else:
            ok["content"] = {_JSON_CT: {"schema": {}}}
        responses = {str(self.status_code): ok}
        if params or schema is not None or form:
            responses["422"] = {
                "description": "Validation Error",
                "content": {_JSON_CT: {"schema": {"$ref": "#/components/schemas/HTTPValidationError"}}},
            }
        op["responses"] = responses
        if comps:
            op["x-veil-components"] = comps
        return op

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
    ):
        self.endpoint = func if inspect.isclass(func) and issubclass(func, WebSocketEndpoint) else None
        target = func if self.endpoint is None else (lambda websocket: None)
        super().__init__(target, path, ["GET"], True, None, tags, summary, 101)
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
    raise TypeError("TOML に変換できない値: %r" % (v,))


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

    def __init__(self, title="veil API", version="0.1.0", lifespan=None, on_startup=None, on_shutdown=None):
        """Create an application with the OpenAPI ``title`` and ``version``.

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
                WebSocketRoute(func, path, receive, send, subprotocols, tags, summary, include_in_schema, validate)
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

    async def _shutdown(self):
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
                any(p.kind in ("form", "file") for p in r.params),
                r.compress,
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
