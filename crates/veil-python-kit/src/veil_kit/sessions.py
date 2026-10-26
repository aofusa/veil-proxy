"""Sessions.

Without a store the session is a signed cookie in the format of Starlette's ``SessionMiddleware`` (base64 JSON signed
with itsdangerous' ``TimestampSigner``), so both can read each other's cookies. With a store (:class:`MemoryStore`,
:class:`RedisStore` or your own :class:`SessionStore`) the cookie holds a signed random id and the data stays on the
server.

A session is a dependency (veil has no ASGI middleware for native routes):

.. code-block:: python

    from veil_kit.sessions import Session, Sessions

    sessions = Sessions(secret_key="change-me")

    @app.get("/count")
    def count(session: Session = veil.Depends(sessions)):
        session["n"] = session.get("n", 0) + 1
        return {"n": session["n"]}

The cookie is written only when the session changed (``Set-Cookie`` on the response of that request).
"""

# F-268: Starlette の SessionMiddleware と同じ Cookie の形式。依存（Depends）として使う。

import base64
import json
import secrets
import threading
import time

import veil

from .cookies import BadSignature, Signer, _request_cookies

__all__ = ["Session", "Sessions", "SessionStore", "MemoryStore", "RedisStore"]


class Session(dict):
    """The data of one session (a ``dict``). ``clear()`` empties it; :meth:`regenerate` gives it a new id."""

    def __init__(self, data=None, sid=None):
        super().__init__(data or {})
        self.sid = sid
        self.modified = False
        self.new_id = False

    def _touch(self):
        self.modified = True

    def __setitem__(self, k, v):
        super().__setitem__(k, v)
        self._touch()

    def __delitem__(self, k):
        super().__delitem__(k)
        self._touch()

    def clear(self):
        """Remove every key (the cookie is deleted at the end of the request)."""
        super().clear()
        self._touch()

    def pop(self, *a):
        """Like ``dict.pop``."""
        r = super().pop(*a)
        self._touch()
        return r

    def popitem(self):
        """Like ``dict.popitem``."""
        r = super().popitem()
        self._touch()
        return r

    def setdefault(self, k, default=None):
        """Like ``dict.setdefault``."""
        if k not in self:
            self._touch()
        return super().setdefault(k, default)

    def update(self, *a, **kw):
        """Like ``dict.update``."""
        super().update(*a, **kw)
        self._touch()

    def regenerate(self):
        """Give the session a new id (after a login; only with a store)."""
        self.new_id = True
        self._touch()


class SessionStore:
    """Server-side storage of session data (implement ``load``, ``save`` and ``delete``; they may be called from
    several threads)."""

    def load(self, sid):
        """Return the data of ``sid`` (a dict) or ``None``."""
        raise NotImplementedError

    def save(self, sid, data, max_age):
        """Store ``data`` for ``sid`` for ``max_age`` seconds."""
        raise NotImplementedError

    def delete(self, sid):
        """Forget ``sid``."""
        raise NotImplementedError


class MemoryStore(SessionStore):
    """Sessions in the process memory (one process; lost on restart)."""

    def __init__(self):
        """Create an empty store."""
        self._data = {}
        self._lock = threading.Lock()

    def load(self, sid):
        """Return the data of ``sid`` unless it expired."""
        with self._lock:
            v = self._data.get(sid)
            if v is None:
                return None
            data, until = v
            if until is not None and until < time.time():
                del self._data[sid]
                return None
            return dict(data)

    def save(self, sid, data, max_age):
        """Store a copy of ``data``."""
        with self._lock:
            self._data[sid] = (dict(data), time.time() + max_age if max_age else None)
            if len(self._data) > 1024 and len(self._data) % 1024 == 0:
                now = time.time()
                for k in [k for k, (_, u) in self._data.items() if u is not None and u < now]:
                    del self._data[k]

    def delete(self, sid):
        """Forget ``sid``."""
        with self._lock:
            self._data.pop(sid, None)


class RedisStore(SessionStore):
    """Sessions in Redis (``pip install "veil-api-kit[redis]"``); ``client`` is a ``redis.Redis``."""

    def __init__(self, client=None, url="redis://localhost:6379/0", prefix="veil:session:"):
        """Use ``client``, or connect to ``url``."""
        if client is None:
            import redis  # noqa: PLC0415

            client = redis.Redis.from_url(url)
        self.client = client
        self.prefix = prefix

    def load(self, sid):
        """Return the data of ``sid``."""
        raw = self.client.get(self.prefix + sid)
        return None if raw is None else json.loads(raw)

    def save(self, sid, data, max_age):
        """Store ``data`` with an expiry of ``max_age`` seconds."""
        self.client.set(self.prefix + sid, json.dumps(data), ex=int(max_age) if max_age else None)

    def delete(self, sid):
        """Forget ``sid``."""
        self.client.delete(self.prefix + sid)


class Sessions:
    """The session dependency: ``session: Session = Depends(sessions)``.

    Arguments are those of Starlette's ``SessionMiddleware``: ``secret_key``, ``session_cookie`` (``"session"``),
    ``max_age`` (14 days; ``None`` makes a browser-session cookie), ``path``, ``same_site`` (``"lax"``),
    ``https_only`` (adds ``Secure``) and ``domain``; plus ``store`` for server-side data.
    """

    _veil_inline = True

    def __init__(self, secret_key, session_cookie="session", max_age=14 * 24 * 60 * 60, path="/", same_site="lax",
                 https_only=False, domain=None, store=None):
        """Create the dependency (see the class documentation)."""
        self.signer = Signer(secret_key)
        self.cookie = session_cookie
        self.max_age = max_age
        self.path = path
        self.same_site = same_site
        self.secure = https_only
        self.domain = domain
        self.store = store

    def _load(self, request):
        raw = _request_cookies(request).get(self.cookie)
        if raw is None:
            return Session()
        try:
            value = self.signer.unsign(raw, self.max_age)
        except BadSignature:
            return Session()
        if self.store is None:
            try:
                return Session(json.loads(base64.b64decode(value)))
            except ValueError:
                return Session()
        sid = value.decode("ascii", errors="replace")
        data = self.store.load(sid)
        return Session(data, sid) if data is not None else Session()

    def _cookie_options(self):
        opts = {"path": self.path, "httponly": True, "samesite": self.same_site, "secure": self.secure}
        if self.domain:
            opts["domain"] = self.domain
        if self.max_age is not None:
            opts["max_age"] = int(self.max_age)
        return opts

    def _write(self, session, response):
        if not session.modified:
            return
        if self.store is None:
            if not session:
                response.delete_cookie(self.cookie, path=self.path, domain=self.domain)
                return
            value = base64.b64encode(json.dumps(dict(session)).encode("utf-8"))
            response.set_cookie(self.cookie, self.signer.sign(value).decode("ascii"), **self._cookie_options())
            return
        if session.sid and (session.new_id or not session):
            self.store.delete(session.sid)
            session.sid = None
        if not session:
            response.delete_cookie(self.cookie, path=self.path, domain=self.domain)
            return
        if session.sid is None:
            session.sid = secrets.token_urlsafe(32)
        self.store.save(session.sid, dict(session), self.max_age)
        response.set_cookie(self.cookie, self.signer.sign(session.sid).decode("ascii"), **self._cookie_options())

    def __call__(self, request: veil.Request, response: veil.Response):
        """Load the session of ``request``; after the handler, write the cookie to ``response`` if it changed."""
        session = self._load(request)
        yield session
        self._write(session, response)
