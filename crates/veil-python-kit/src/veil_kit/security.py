"""Authentication with the classes and behaviour of ``fastapi.security``.

Each scheme is a dependency (``Depends(scheme)`` or ``Security(scheme, scopes=[...])``) that reads the credentials
from the request and appears in OpenAPI ``securitySchemes``. With ``auto_error=False`` a missing credential gives
``None`` instead of a 401.

.. code-block:: python

    from veil_kit.security import OAuth2PasswordBearer, JWT

    oauth2 = OAuth2PasswordBearer(tokenUrl="/token")
    tokens = JWT(secret="change-me", algorithms=["HS256"])     # needs "veil-api-kit[jwt]"

    def current_user(token: str = veil.Depends(oauth2)):
        return tokens.decode(token)["sub"]

    @app.get("/me")
    def me(user: str = veil.Depends(current_user)):
        return {"user": user}
"""

# F-268: fastapi.security と同じ名前・同じ振る舞い（401 と WWW-Authenticate）

import base64
import binascii
import hmac

import veil
from veil import HTTPException

from .cookies import _request_cookies

__all__ = [
    "HTTPBasic",
    "HTTPBasicCredentials",
    "HTTPBearer",
    "HTTPDigest",
    "HTTPAuthorizationCredentials",
    "APIKeyHeader",
    "APIKeyCookie",
    "APIKeyQuery",
    "OAuth2PasswordBearer",
    "OAuth2AuthorizationCodeBearer",
    "OAuth2PasswordRequestForm",
    "OAuth2PasswordRequestFormStrict",
    "SecurityScopes",
    "JWT",
    "compare_digest",
    "requires_scopes",
]

SecurityScopes = veil.SecurityScopes


def compare_digest(a, b):
    """Compare two strings in constant time."""
    return hmac.compare_digest(a.encode() if isinstance(a, str) else a, b.encode() if isinstance(b, str) else b)


def _unauthorized(detail="Not authenticated", scheme="Bearer"):
    return HTTPException(401, detail, {"WWW-Authenticate": scheme})


class _Scheme:
    # 要求ごとにスレッドを跨がず、その場で呼んでよい（軽い）
    _veil_inline = True
    scheme_name = None

    def openapi_scheme(self):
        """``(name, Security Scheme Object)`` for OpenAPI."""
        return self.scheme_name or type(self).__name__, self.model


class HTTPAuthorizationCredentials:
    """``scheme`` and ``credentials`` of an ``Authorization`` header."""

    def __init__(self, scheme, credentials):
        self.scheme = scheme
        self.credentials = credentials

    def __repr__(self):
        return "HTTPAuthorizationCredentials(scheme=%r)" % (self.scheme,)


class HTTPBasicCredentials:
    """``username`` and ``password`` of HTTP Basic authentication."""

    def __init__(self, username, password):
        self.username = username
        self.password = password

    def __repr__(self):
        return "HTTPBasicCredentials(username=%r)" % (self.username,)


def _authorization(request):
    v = request.headers.get("authorization")
    if not v:
        return None, None
    scheme, _, cred = v.partition(" ")
    return scheme, cred.strip()


class HTTPBasic(_Scheme):
    """HTTP Basic authentication; gives :class:`HTTPBasicCredentials`."""

    def __init__(self, *, scheme_name=None, realm=None, description=None, auto_error=True):
        """``realm`` goes to ``WWW-Authenticate``."""
        self.scheme_name = scheme_name
        self.realm = realm
        self.auto_error = auto_error
        self.model = {"type": "http", "scheme": "basic"}
        if description:
            self.model["description"] = description

    def __call__(self, request: veil.Request):
        """Read the credentials (401 with ``WWW-Authenticate: Basic`` when absent or malformed)."""
        challenge = 'Basic realm="%s"' % self.realm if self.realm else "Basic"
        scheme, cred = _authorization(request)
        if not scheme or scheme.lower() != "basic":
            if self.auto_error:
                raise _unauthorized(scheme=challenge)
            return None
        try:
            user, sep, pw = base64.b64decode(cred).decode("utf-8").partition(":")
        except (ValueError, UnicodeDecodeError, binascii.Error):
            raise _unauthorized("Invalid authentication credentials", challenge)
        if not sep:
            raise _unauthorized("Invalid authentication credentials", challenge)
        return HTTPBasicCredentials(user, pw)


class HTTPBearer(_Scheme):
    """``Authorization: Bearer <token>``; gives :class:`HTTPAuthorizationCredentials`."""

    def __init__(self, *, bearerFormat=None, scheme_name=None, description=None, auto_error=True):  # noqa: N803
        """``bearerFormat`` (such as ``"JWT"``) goes to OpenAPI."""
        self.scheme_name = scheme_name
        self.auto_error = auto_error
        self.model = {"type": "http", "scheme": "bearer"}
        if bearerFormat:
            self.model["bearerFormat"] = bearerFormat
        if description:
            self.model["description"] = description

    def __call__(self, request: veil.Request):
        """Read the token (401 with ``WWW-Authenticate: Bearer`` when absent)."""
        scheme, cred = _authorization(request)
        if not scheme or not cred or scheme.lower() != "bearer":
            if self.auto_error:
                raise _unauthorized()
            return None
        return HTTPAuthorizationCredentials(scheme, cred)


class HTTPDigest(_Scheme):
    """``Authorization: Digest ...`` (the header is passed through; checking it is up to you)."""

    def __init__(self, *, scheme_name=None, description=None, auto_error=True):
        """Declare the scheme."""
        self.scheme_name = scheme_name
        self.auto_error = auto_error
        self.model = {"type": "http", "scheme": "digest"}
        if description:
            self.model["description"] = description

    def __call__(self, request: veil.Request):
        """Read the header (401 when absent)."""
        scheme, cred = _authorization(request)
        if not scheme or scheme.lower() != "digest":
            if self.auto_error:
                raise _unauthorized(scheme="Digest")
            return None
        return HTTPAuthorizationCredentials(scheme, cred)


class _APIKey(_Scheme):
    where = None

    def __init__(self, *, name, scheme_name=None, description=None, auto_error=True):
        self.name = name
        self.scheme_name = scheme_name
        self.auto_error = auto_error
        self.model = {"type": "apiKey", "in": self.where, "name": name}
        if description:
            self.model["description"] = description

    def _value(self, request):
        raise NotImplementedError

    def __call__(self, request: veil.Request):
        """The key (401 when absent)."""
        v = self._value(request)
        if not v:
            if self.auto_error:
                raise _unauthorized(scheme="APIKey")
            return None
        return v


class APIKeyHeader(_APIKey):
    """An API key in a header (``name`` is the header name)."""

    where = "header"

    def _value(self, request):
        return request.headers.get(self.name)


class APIKeyCookie(_APIKey):
    """An API key in a cookie."""

    where = "cookie"

    def _value(self, request):
        return _request_cookies(request).get(self.name)


class APIKeyQuery(_APIKey):
    """An API key in the query string."""

    where = "query"

    def _value(self, request):
        return request.query_params.get(self.name)


class OAuth2PasswordBearer(_Scheme):
    """OAuth2 password flow: reads the bearer token; ``tokenUrl`` is where clients get it (for OpenAPI)."""

    def __init__(self, tokenUrl, scheme_name=None, scopes=None, description=None, auto_error=True,  # noqa: N803
                 refreshUrl=None):  # noqa: N803
        """``scopes`` maps scope names to descriptions."""
        self.scheme_name = scheme_name
        self.auto_error = auto_error
        flow = {"tokenUrl": tokenUrl, "scopes": dict(scopes or {})}
        if refreshUrl:
            flow["refreshUrl"] = refreshUrl
        self.model = {"type": "oauth2", "flows": {"password": flow}}
        if description:
            self.model["description"] = description

    def __call__(self, request: veil.Request):
        """The token (``str``; 401 with ``WWW-Authenticate: Bearer`` when absent)."""
        scheme, cred = _authorization(request)
        if not scheme or not cred or scheme.lower() != "bearer":
            if self.auto_error:
                raise _unauthorized()
            return None
        return cred


class OAuth2AuthorizationCodeBearer(OAuth2PasswordBearer):
    """OAuth2 authorization code flow (``authorizationUrl`` and ``tokenUrl`` for OpenAPI)."""

    def __init__(self, authorizationUrl, tokenUrl, refreshUrl=None, scheme_name=None, scopes=None,  # noqa: N803
                 description=None, auto_error=True):
        """Declare the flow."""
        super().__init__(tokenUrl, scheme_name, scopes, description, auto_error)
        flow = {"authorizationUrl": authorizationUrl, "tokenUrl": tokenUrl, "scopes": dict(scopes or {})}
        if refreshUrl:
            flow["refreshUrl"] = refreshUrl
        self.model = {"type": "oauth2", "flows": {"authorizationCode": flow}}


class OAuth2PasswordRequestForm:
    """The form of the OAuth2 password flow (``username``, ``password``, ``scope`` → ``scopes``, ``grant_type``,
    ``client_id``, ``client_secret``); use as ``form: OAuth2PasswordRequestForm = Depends()``."""

    _veil_inline = True

    def __init__(
        self,
        grant_type: str = veil.Form(None),
        username: str = veil.Form(),
        password: str = veil.Form(),
        scope: str = veil.Form(""),
        client_id: str = veil.Form(None),
        client_secret: str = veil.Form(None),
    ):
        self.grant_type = grant_type
        self.username = username
        self.password = password
        self.scopes = scope.split()
        self.client_id = client_id
        self.client_secret = client_secret


class OAuth2PasswordRequestFormStrict(OAuth2PasswordRequestForm):
    """Like :class:`OAuth2PasswordRequestForm`, with ``grant_type`` required to be ``"password"``."""

    def __init__(
        self,
        grant_type: str = veil.Form(),
        username: str = veil.Form(),
        password: str = veil.Form(),
        scope: str = veil.Form(""),
        client_id: str = veil.Form(None),
        client_secret: str = veil.Form(None),
    ):
        if grant_type != "password":
            raise HTTPException(422, "grant_type must be 'password'")
        super().__init__(grant_type, username, password, scope, client_id, client_secret)


def requires_scopes(scheme, verify):
    """A dependency checking the scopes of a :class:`veil.Security` declaration.

    ``verify(token)`` returns the scopes the credential grants (an iterable) or raises; the dependency answers 401
    when one of the required scopes is missing, with the scopes in ``WWW-Authenticate``.
    """

    def check(security_scopes: veil.SecurityScopes, token=veil.Depends(scheme)):
        granted = set(verify(token) or ())
        missing = [s for s in security_scopes.scopes if s not in granted]
        if missing:
            raise HTTPException(401, "Not enough permissions",
                                {"WWW-Authenticate": 'Bearer scope="%s"' % security_scopes.scope_str})
        return token

    check._veil_inline = True
    return check


class JWT:
    """Issue and verify JSON Web Tokens with PyJWT (``pip install "veil-api-kit[jwt]"``).

    ``key`` is the secret (HMAC) or the public key (RSA, EC, EdDSA) used to verify; ``signing_key`` signs (defaults to
    ``key``). ``audience``, ``issuer`` and ``leeway`` are checked on :meth:`decode`. As in PyJWT, a token with an
    ``aud`` claim is rejected when no ``audience`` is given (it was issued for another service);
    ``verify_audience=False`` turns the ``aud`` check off.
    """

    # B-201: aud の検査は PyJWT の既定どおり（audience 無しで aud 付きのトークンは拒否）。外すときは明示する
    def __init__(self, key=None, algorithms=("HS256",), *, secret=None, signing_key=None, audience=None, issuer=None,
                 leeway=0, jwks_url=None, verify_audience=True):
        """Create a verifier (``jwks_url`` fetches the keys from a JWK Set by ``kid``)."""
        import jwt  # noqa: PLC0415

        self._jwt = jwt
        self.key = key if key is not None else secret
        self.signing_key = signing_key if signing_key is not None else self.key
        self.algorithms = list(algorithms)
        self.audience = audience
        self.issuer = issuer
        self.leeway = leeway
        self.verify_audience = verify_audience
        self._jwks = jwt.PyJWKClient(jwks_url) if jwks_url else None

    def encode(self, claims, algorithm=None, headers=None):
        """Sign ``claims`` (a dict; ``exp`` may be a ``datetime``)."""
        return self._jwt.encode(dict(claims), self.signing_key, algorithm=algorithm or self.algorithms[0],
                                headers=headers)

    def decode(self, token, *, verify_exp=True):
        """Verify ``token`` and return its claims; raise ``HTTPException(401)`` when it is not valid."""
        jwt = self._jwt
        try:
            key = self._jwks.get_signing_key_from_jwt(token).key if self._jwks is not None else self.key
            return jwt.decode(token, key, algorithms=self.algorithms, audience=self.audience, issuer=self.issuer,
                              leeway=self.leeway, options={"verify_exp": verify_exp,
                                                           "verify_aud": self.verify_audience})
        except jwt.PyJWTError as e:
            raise HTTPException(401, "Could not validate credentials: %s" % e,
                                {"WWW-Authenticate": 'Bearer error="invalid_token"'})

    def dependency(self, scheme=None):
        """A dependency returning the verified claims of the bearer token (``Depends(tokens.dependency())``)."""
        scheme = scheme or HTTPBearer(bearerFormat="JWT")
        me = self

        def claims(credentials=veil.Depends(scheme)):
            token = credentials.credentials if isinstance(credentials, HTTPAuthorizationCredentials) else credentials
            return me.decode(token)

        claims._veil_inline = True
        return claims
