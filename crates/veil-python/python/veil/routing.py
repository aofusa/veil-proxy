"""``APIRouter``: groups of routes with a common prefix, tags and dependencies (as in FastAPI)."""

# F-268

__all__ = ["APIRouter"]


class APIRouter:
    """Routes registered with the same decorators as :class:`veil.App`, added to an application with
    ``app.include_router(router)``. ``prefix``, ``tags`` and ``dependencies`` apply to every route of the router."""

    def __init__(self, prefix="", tags=None, dependencies=None, responses=None):
        """Create a router with a path ``prefix`` and the ``tags`` / ``dependencies`` / ``responses`` of its routes."""
        if prefix and (not prefix.startswith("/") or prefix.endswith("/")):
            raise ValueError("a router prefix must start with '/' and not end with '/'")
        self.prefix = prefix
        self.tags = list(tags or [])
        self.dependencies = list(dependencies or [])
        self.responses = dict(responses or {})
        # ("route" | "websocket", path, kwargs, func)
        self.entries = []

    def route(self, path, methods=None, **kw):
        """Register a handler (same arguments as :meth:`veil.App.route`); use as a decorator."""

        def deco(func):
            self.entries.append(("route", path, dict(kw, methods=methods), func))
            return func

        return deco

    def _verb(self, verb, path, kw):
        kw.pop("methods", None)
        return self.route(path, methods=[verb], **kw)

    def get(self, path, **kw):
        """Register a ``GET`` handler."""
        return self._verb("GET", path, kw)

    def post(self, path, **kw):
        """Register a ``POST`` handler."""
        return self._verb("POST", path, kw)

    def put(self, path, **kw):
        """Register a ``PUT`` handler."""
        return self._verb("PUT", path, kw)

    def delete(self, path, **kw):
        """Register a ``DELETE`` handler."""
        return self._verb("DELETE", path, kw)

    def patch(self, path, **kw):
        """Register a ``PATCH`` handler."""
        return self._verb("PATCH", path, kw)

    def head(self, path, **kw):
        """Register a ``HEAD`` handler."""
        return self._verb("HEAD", path, kw)

    def options(self, path, **kw):
        """Register an ``OPTIONS`` handler."""
        return self._verb("OPTIONS", path, kw)

    def websocket(self, path, **kw):
        """Register a WebSocket handler (same arguments as :meth:`veil.App.websocket`)."""

        def deco(func):
            self.entries.append(("websocket", path, kw, func))
            return func

        return deco

    def include_router(self, router, prefix="", tags=None, dependencies=None, responses=None):
        """Add the routes of another router under ``prefix``."""
        for kind, path, kw, func in router._expanded(prefix, tags, dependencies, responses):
            self.entries.append((kind, path, kw, func))

    def _expanded(self, prefix="", tags=None, dependencies=None, responses=None):
        """The entries with the prefixes, tags, dependencies and responses of this router (and the caller's) applied."""
        outer_tags = list(tags or []) + self.tags
        outer_deps = list(dependencies or []) + self.dependencies
        outer_resp = dict(responses or {})
        outer_resp.update(self.responses)
        for kind, path, kw, func in self.entries:
            kw = dict(kw)
            full = prefix + self.prefix + path
            if kind == "route":
                kw["tags"] = outer_tags + list(kw.get("tags") or [])
                kw["dependencies"] = outer_deps + list(kw.get("dependencies") or [])
                r = dict(outer_resp)
                r.update(kw.get("responses") or {})
                kw["responses"] = r
            else:
                kw["tags"] = outer_tags + list(kw.get("tags") or [])
                if outer_deps:
                    kw["dependencies"] = outer_deps + list(kw.get("dependencies") or [])
            yield kind, full, kw, func
