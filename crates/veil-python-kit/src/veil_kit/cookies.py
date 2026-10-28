"""Signed cookies.

:class:`Signer` produces and checks the format of itsdangerous' ``TimestampSigner`` (``value.timestamp.signature``,
HMAC-SHA1 with the ``django-concat`` key derivation), so cookies written by Flask or Starlette's ``SessionMiddleware``
with the same secret are accepted and the other way round. Several secrets rotate keys: the last one signs, all of them
verify.

.. code-block:: python

    from veil_kit.cookies import SignedCookies

    cookies = SignedCookies("change-me", max_age=3600)

    @app.get("/login")
    def login(response: veil.Response):
        cookies.set(response, "user", "alice", httponly=True)
        return "ok"

    @app.get("/me")
    def me(request: veil.Request):
        return {"user": cookies.get(request, "user")}
"""

# F-268: itsdangerous の TimestampSigner と同じ形式（Starlette の SessionMiddleware の Cookie と互換）

import base64
import hashlib
import hmac
import time

__all__ = ["Signer", "BadSignature", "SignatureExpired", "SignedCookies", "parse_cookies"]


class BadSignature(Exception):
    """The value was not signed with one of the secrets (or was changed)."""


class SignatureExpired(BadSignature):
    """The signature is valid but older than ``max_age``."""


def _b64e(b):
    return base64.urlsafe_b64encode(b).rstrip(b"=")


def _b64d(b):
    return base64.urlsafe_b64decode(b + b"=" * (-len(b) % 4))


def _int_to_bytes(n):
    return n.to_bytes((n.bit_length() + 7) // 8 or 1, "big")


class Signer:
    """Signs values with a timestamp (the format of itsdangerous' ``TimestampSigner``)."""

    def __init__(self, secret_key, salt=b"itsdangerous.Signer", sep=b"."):
        """``secret_key`` is a ``str`` / ``bytes`` or a list of them (oldest first)."""
        keys = secret_key if isinstance(secret_key, (list, tuple)) else [secret_key]
        if not keys:
            raise ValueError("at least one secret key is needed")
        salt = salt.encode() if isinstance(salt, str) else salt
        self.sep = sep
        # django-concat: sha1(salt + b"signer" + secret)
        self._keys = [hashlib.sha1(salt + b"signer" + (k.encode() if isinstance(k, str) else k)).digest() for k in keys]

    def _sig(self, key, value):
        return _b64e(hmac.new(key, value, hashlib.sha1).digest())

    def sign(self, value):
        """Return ``value.timestamp.signature`` (``bytes``)."""
        value = value.encode() if isinstance(value, str) else value
        v = value + self.sep + _b64e(_int_to_bytes(int(time.time())))
        return v + self.sep + self._sig(self._keys[-1], v)

    def unsign(self, signed, max_age=None):
        """Return the value of ``signed``; raise :class:`BadSignature` or :class:`SignatureExpired`."""
        signed = signed.encode() if isinstance(signed, str) else signed
        v, sep, sig = signed.rpartition(self.sep)
        if not sep:
            raise BadSignature("no separator")
        if not any(hmac.compare_digest(self._sig(k, v), sig) for k in reversed(self._keys)):
            raise BadSignature("signature does not match")
        value, sep, ts = v.rpartition(self.sep)
        if not sep:
            raise BadSignature("no timestamp")
        try:
            t = int.from_bytes(_b64d(ts), "big")
        except Exception:  # noqa: BLE001
            raise BadSignature("bad timestamp")
        if max_age is not None and time.time() - t > max_age:
            raise SignatureExpired("signature expired")
        return value


def parse_cookies(header):
    """Parse a ``Cookie`` header into a dict (the first value of a repeated name wins; quotes are removed)."""
    out = {}
    for part in (header or "").split(";"):
        name, eq, value = part.partition("=")
        if not eq:
            continue
        name = name.strip()
        value = value.strip()
        if len(value) >= 2 and value[0] == value[-1] == '"':
            value = value[1:-1]
        if name and name not in out:
            out[name] = value
    return out


def _request_cookies(request):
    c = getattr(request, "cookies", None)
    if c is not None:
        return c
    return parse_cookies(", ".join(request.headers.getlist("cookie")).replace(", ", "; "))


class SignedCookies:
    """Reads and writes cookies signed by a :class:`Signer`."""

    def __init__(self, secret_key, max_age=None, salt="veil_kit.cookies"):
        """``max_age`` (seconds) rejects older signatures; ``salt`` separates the uses of one secret."""
        self.signer = Signer(secret_key, salt=salt)
        self.max_age = max_age

    def get(self, request, name, default=None):
        """The verified value of cookie ``name`` (``default`` when absent, unsigned, changed or expired)."""
        raw = _request_cookies(request).get(name)
        if raw is None:
            return default
        try:
            return self.signer.unsign(raw, self.max_age).decode("utf-8")
        except (BadSignature, UnicodeDecodeError):
            return default

    def set(self, response, name, value, **options):
        """Set cookie ``name`` to the signed ``value`` (``options`` as for ``Response.set_cookie``)."""
        if self.max_age is not None:
            options.setdefault("max_age", int(self.max_age))
        response.set_cookie(name, self.signer.sign(str(value)).decode("ascii"), **options)

    def delete(self, response, name, **options):
        """Delete cookie ``name``."""
        response.delete_cookie(name, **options)
