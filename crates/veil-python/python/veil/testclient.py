"""``TestClient``: call the routes of an application in-process, without a server (like Starlette's).

.. code-block:: python

    from veil.testclient import TestClient

    client = TestClient(app)
    r = client.get("/items/1", params={"q": "x"})
    assert r.status_code == 200 and r.json() == {...}

Routing, argument binding, dependencies (``app.dependency_overrides`` included), validation, exceptions, background
tasks and mounted ASGI applications work as in the server. What the Rust server adds is not applied: compression,
CORS (``app.cors``), static files (``app.serve_dir`` / ``StaticFiles``), body size limits and WebSocket routes. Use a
real server (``app.run`` in a subprocess) to test those.
"""

# F-268: サーバ無しでルートを呼ぶ試験用クライアント（Starlette の TestClient と同じ使い方）。

import http.cookies
import json as _json
import re
import urllib.parse
import uuid

__all__ = ["TestClient", "TestResponse"]


class TestResponse:
    """The response of a :class:`TestClient` request."""

    __test__ = False

    def __init__(self, status_code, headers, content, url):
        self.status_code = status_code
        self.headers = _Headers(headers)
        self.content = content
        self.url = url

    @property
    def text(self):
        """The body decoded as UTF-8."""
        return self.content.decode("utf-8", errors="replace")

    def json(self):
        """The body parsed as JSON."""
        return _json.loads(self.content)

    @property
    def is_success(self):
        """True for 2xx."""
        return 200 <= self.status_code < 300

    @property
    def cookies(self):
        """The cookies set by the response (``Set-Cookie``), as a dict."""
        out = {}
        for v in self.headers.get_list("set-cookie"):
            c = http.cookies.SimpleCookie()
            try:
                c.load(v)
            except http.cookies.CookieError:
                continue
            for k, m in c.items():
                out[k] = m.value
        return out

    def raise_for_status(self):
        """Raise ``AssertionError`` unless the status is 2xx or 3xx."""
        if self.status_code >= 400:
            raise AssertionError("HTTP %d: %s" % (self.status_code, self.text[:200]))

    def __repr__(self):
        return "<TestResponse [%d]>" % self.status_code


class _Headers:
    def __init__(self, items):
        self._items = [(str(k), str(v)) for k, v in items]

    def get(self, name, default=None):
        low = name.lower()
        for k, v in self._items:
            if k.lower() == low:
                return v
        return default

    def get_list(self, name):
        low = name.lower()
        return [v for k, v in self._items if k.lower() == low]

    def __getitem__(self, name):
        v = self.get(name)
        if v is None:
            raise KeyError(name)
        return v

    def __contains__(self, name):
        return self.get(name) is not None

    def items(self):
        return list(self._items)

    def __repr__(self):
        return "Headers(%r)" % (self._items,)


_PARAM_RE = re.compile(r"\{(\*?)([^}/:]+)(?::[^}]*)?\}")


def _compile(path):
    """A regex for a route path and its specificity (static segments first, catch-alls last)."""
    out = ["^"]
    pos = 0
    statics = 0
    catch = False
    for m in _PARAM_RE.finditer(path):
        lit = path[pos:m.start()]
        statics += len([s for s in lit.split("/") if s])
        out.append(re.escape(lit))
        if m.group(1):
            out.append("(?P<%s>.*)" % m.group(2))
            catch = True
        else:
            out.append("(?P<%s>[^/]+)" % m.group(2))
        pos = m.end()
    lit = path[pos:]
    statics += len([s for s in lit.split("/") if s])
    out.append(re.escape(lit))
    out.append("$")
    return re.compile("".join(out)), (catch, -statics, -len(path))


def _multipart(data, files):
    boundary = "veil-test-" + uuid.uuid4().hex
    parts = []
    for k, v in (data or {}).items():
        for one in v if isinstance(v, (list, tuple)) else [v]:
            parts.append(("--%s\r\nContent-Disposition: form-data; name=\"%s\"\r\n\r\n" % (boundary, k)).encode()
                         + str(one).encode() + b"\r\n")
    items = files.items() if isinstance(files, dict) else files
    for k, f in items:
        if isinstance(f, tuple):
            name, content = f[0], f[1]
            ctype = f[2] if len(f) > 2 else "application/octet-stream"
        else:
            name, content, ctype = getattr(f, "name", k), f, "application/octet-stream"
        if hasattr(content, "read"):
            content = content.read()
        if isinstance(content, str):
            content = content.encode()
        parts.append(("--%s\r\nContent-Disposition: form-data; name=\"%s\"; filename=\"%s\"\r\nContent-Type: %s\r\n\r\n"
                      % (boundary, k, name, ctype)).encode() + content + b"\r\n")
    parts.append(("--%s--\r\n" % boundary).encode())
    return b"".join(parts), "multipart/form-data; boundary=" + boundary


class TestClient:
    """Calls the routes of ``app`` in-process. Use it as a context manager to run the startup and shutdown
    handlers (``lifespan``) around the requests."""

    __test__ = False

    def __init__(self, app, base_url="http://testserver", raise_server_exceptions=True, headers=None, cookies=None,
                 follow_redirects=True):
        """Create a client for ``app`` (a :class:`veil.App`)."""
        self.app = app
        self.base_url = base_url.rstrip("/")
        self.raise_server_exceptions = raise_server_exceptions
        self.headers = dict(headers or {})
        self.cookies = dict(cookies or {})
        self.follow_redirects = follow_redirects
        self._table = None
        self._mounts = None

    # ---- lifespan ----

    def __enter__(self):
        from . import _run_coro

        _run_coro(self.app._startup())
        return self

    def __exit__(self, *exc):
        from . import _run_coro

        _run_coro(self.app._shutdown())

    # ---- ルーティング ----

    def _routes(self):
        if self._table is None:
            table = []
            for r in self.app.routes:
                rx, key = _compile(r.path)
                table.append((key, rx, r))
            table.sort(key=lambda t: t[0])
            self._table = table
            self._mounts = sorted(self.app.asgi_mounts, key=lambda m: -len(m.prefix))
        return self._table

    def _match(self, method, path):
        allowed = False
        for _, rx, r in self._routes():
            m = rx.match(path)
            if m is None:
                continue
            if method in r.methods or (method == "HEAD" and "GET" in r.methods):
                return r, m.groupdict()
            allowed = True
        for a in self._mounts:
            if a.prefix == "" or path == a.prefix or path.startswith(a.prefix + "/"):
                return a, {}
        return (405 if allowed else 404), None

    # ---- 要求 ----

    def request(self, method, url, *, params=None, json=None, data=None, content=None, files=None, headers=None,
                cookies=None, follow_redirects=None):
        """Send a request and return a :class:`TestResponse`."""
        from . import HTTPException, JSONResponse, Request, Response, StreamingResponse

        method = method.upper()
        parsed = urllib.parse.urlsplit(url)
        path = parsed.path or "/"
        query = parsed.query
        if params:
            extra = urllib.parse.urlencode(params, doseq=True)
            query = query + "&" + extra if query else extra
        hs = dict(self.headers)
        hs.update(headers or {})
        body = b""
        if json is not None:
            body = _json.dumps(json).encode()
            hs.setdefault("content-type", "application/json")
        elif files is not None:
            body, ctype = _multipart(data, files)
            hs["content-type"] = ctype
        elif isinstance(data, dict):
            body = urllib.parse.urlencode(data, doseq=True).encode()
            hs.setdefault("content-type", "application/x-www-form-urlencoded")
        elif data is not None:
            body = data.encode() if isinstance(data, str) else bytes(data)
        elif content is not None:
            body = content.encode() if isinstance(content, str) else bytes(content)
        jar = dict(self.cookies)
        jar.update(cookies or {})
        if jar:
            hs["cookie"] = "; ".join("%s=%s" % kv for kv in jar.items())
        hs.setdefault("host", urllib.parse.urlsplit(self.base_url).netloc or "testserver")
        hs.setdefault("user-agent", "testclient")
        if body:
            hs.setdefault("content-length", str(len(body)))
        route, pp = self._match(method, path)
        if isinstance(route, int):
            resp = JSONResponse({"detail": "Not Found" if route == 404 else "Method Not Allowed"}, route)
        else:
            req = Request(method, path, query, list(hs.items()), body, pp, "testclient")
            try:
                resp = route.handle(req)
            except HTTPException as e:
                resp = JSONResponse({"detail": e.detail}, e.status_code, e.headers)
            except Exception:
                if self.raise_server_exceptions:
                    raise
                resp = JSONResponse({"detail": "Internal Server Error"}, 500)
        if not isinstance(resp, Response):
            resp = route.to_response(resp)
        body_out = resp.body if not isinstance(resp, StreamingResponse) else resp.body
        if method == "HEAD":
            body_out = b""
        bg = getattr(resp, "background", None)
        if bg is not None:
            bg()
        out = TestResponse(resp.status, resp.headers, body_out, self.base_url + path)
        for k, v in out.cookies.items():
            self.cookies[k] = v
        follow = self.follow_redirects if follow_redirects is None else follow_redirects
        if follow and out.status_code in (301, 302, 303, 307, 308) and out.headers.get("location"):
            loc = out.headers["location"]
            target = urllib.parse.urlsplit(loc)
            if not target.netloc or self.base_url.endswith(target.netloc):
                nm = method if out.status_code in (307, 308) else ("GET" if method != "HEAD" else "HEAD")
                return self.request(nm, target.path + ("?" + target.query if target.query else ""),
                                    headers=headers, follow_redirects=True)
        return out

    def get(self, url, **kw):
        """``GET``."""
        return self.request("GET", url, **kw)

    def post(self, url, **kw):
        """``POST``."""
        return self.request("POST", url, **kw)

    def put(self, url, **kw):
        """``PUT``."""
        return self.request("PUT", url, **kw)

    def patch(self, url, **kw):
        """``PATCH``."""
        return self.request("PATCH", url, **kw)

    def delete(self, url, **kw):
        """``DELETE``."""
        return self.request("DELETE", url, **kw)

    def head(self, url, **kw):
        """``HEAD``."""
        return self.request("HEAD", url, **kw)

    def options(self, url, **kw):
        """``OPTIONS``."""
        return self.request("OPTIONS", url, **kw)
