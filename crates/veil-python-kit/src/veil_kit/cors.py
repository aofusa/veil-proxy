"""CORS with the arguments of Starlette's ``CORSMiddleware``.

.. code-block:: python

    from veil_kit.cors import CORSMiddleware

    app.add_middleware(CORSMiddleware, allow_origins=["https://app.example.com"], allow_credentials=True)

The headers are added by the server (Rust; the same implementation as the Rust ``veil-proxy-kit``): preflight requests
are answered without calling Python, and the header values are built once at startup.
"""

# F-268: 中身は Rust（veil_kit::cors）。Python の層で要求ごとにヘッダを作らない。

__all__ = ["CORSMiddleware"]


class CORSMiddleware:
    """Pass to ``app.add_middleware`` (or call ``app.cors(...)`` with the same keyword arguments).

    ``allow_origins`` lists exact origins or ``https://*.example.com`` patterns (``["*"]`` allows any origin, not with
    ``allow_credentials``); ``allow_methods`` defaults to ``["GET"]`` (``["*"]`` = all), ``allow_headers`` to none,
    ``expose_headers`` to none and ``max_age`` to 600 seconds. ``prefix`` limits CORS to the routes below a path.
    ``allow_origin_regex`` is not supported.
    """

    def __init__(self, app=None, **options):
        self.options = options

    @staticmethod
    def _veil_install(app, **options):
        """Called by ``app.add_middleware``."""
        if options.get("allow_origin_regex"):
            raise ValueError("allow_origin_regex is not supported; list the origins (\"https://*.example.com\" works)")
        options.pop("allow_origin_regex", None)
        app.cors(**options)
