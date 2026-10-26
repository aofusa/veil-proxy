"""Helpers for veil-api applications.

Each module is imported on its own (``from veil_kit.security import OAuth2PasswordBearer``); optional
dependencies are installed with extras (``pip install "veil-api-kit[jwt,graphql,grpc]"``):

* ``veil_kit.cookies`` - signed cookies (itsdangerous-compatible format, key rotation)
* ``veil_kit.sessions`` - cookie sessions (Starlette-compatible) and server-side stores (memory, Redis)
* ``veil_kit.cors`` - ``CORSMiddleware`` on top of the native ``app.cors()``
* ``veil_kit.security`` - HTTP Basic / Bearer / Digest, API keys, OAuth2, JWT (PyJWT)
* ``veil_kit.redirects`` - ``safe_redirect`` to allowed hosts only
* ``veil_kit.staticfiles`` - ``StaticFiles`` served natively (sendfile, no Python per request)
* ``veil_kit.graphql`` - strawberry schemas over HTTP, SSE and WebSocket
* ``veil_kit.grpc`` - a channel for ``grpc_tools`` stubs and the server helpers
* ``veil_kit.di`` - a dependency injection container
* ``veil_kit.tasks`` - periodic, cron and resident tasks
* ``veil_kit.testing`` - ``TestClient`` and dependency overrides
"""

__version__ = "0.9.2"

__all__ = ["__version__"]
