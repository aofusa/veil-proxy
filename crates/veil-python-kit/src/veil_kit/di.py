"""A dependency injection container on top of ``veil.Depends``.

.. code-block:: python

    from veil_kit.di import Container, Inject

    container = Container()
    container.singleton(Settings, lambda: Settings.from_env())
    container.scoped(Session, make_session)              # once per request; make_session may yield
    container.transient(Clock, Clock)

    @container.provider(Repo, scope="scoped")
    def repo(session: Session, settings: Settings):      # parameters of registered types are injected
        return Repo(session, settings)

    @app.get("/items")
    def items(repo: Repo = Inject(Repo)):
        return repo.list()

    with container.override(Repo, FakeRepo()):           # tests
        ...

A provider is resolved through ``veil.Depends``: its parameters may also be request parameters (``Query``,
``Header``, ``request: veil.Request``, other ``Depends``) and generators clean up after the request. ``singleton``
values are created once per process (thread-safe), ``scoped`` once per request, ``transient`` on every use.
Registrations may come after the routes that use them, until the server starts.
"""

# F-268: 解決は veil の Depends の木（登録時に組み立て、要求ごとは決まった順に呼ぶ）。差し替えは提供者の入れ替え。

import contextlib
import inspect
import threading

import veil

__all__ = ["Container", "Inject", "SINGLETON", "SCOPED", "TRANSIENT"]

SINGLETON, SCOPED, TRANSIENT = "singleton", "scoped", "transient"

_current = None


class _Registration:
    __slots__ = ("key", "factory", "scope", "value", "lock", "has_value", "resolver")

    def __init__(self, key, factory, scope):
        self.key = key
        self.factory = factory
        self.scope = scope
        self.value = None
        self.has_value = False
        self.lock = threading.Lock()
        self.resolver = None


class Container:
    """Providers by type (or any hashable key)."""

    def __init__(self):
        """Create an empty container."""
        self._regs = {}
        self._overrides = {}
        self._stubs = {}
        global _current
        _current = self

    # ---- 登録 ----

    def register(self, key, factory=None, scope=SCOPED):
        """Register ``factory`` (a callable, or a value for ``singleton``) for ``key``; ``factory`` defaults to ``key``."""
        if scope not in (SINGLETON, SCOPED, TRANSIENT):
            raise ValueError("scope must be singleton, scoped or transient")
        if factory is None:
            factory = key
        if not callable(factory):
            value = factory
            reg = _Registration(key, None, SINGLETON)
            reg.value, reg.has_value = value, True
        else:
            reg = _Registration(key, factory, scope)
        self._regs[key] = reg
        self._stubs.pop(key, None)
        return self

    def singleton(self, key, factory=None):
        """Register a provider created once per process (or a ready value)."""
        return self.register(key, factory, SINGLETON)

    def scoped(self, key, factory=None):
        """Register a provider created once per request."""
        return self.register(key, factory, SCOPED)

    def transient(self, key, factory=None):
        """Register a provider created on every use."""
        return self.register(key, factory, TRANSIENT)

    def value(self, key, value):
        """Register an existing value."""
        reg = _Registration(key, None, SINGLETON)
        reg.value, reg.has_value = value, True
        self._regs[key] = reg
        self._stubs.pop(key, None)
        return self

    def provider(self, key, scope=SCOPED):
        """Decorator form of :meth:`register`."""

        def deco(f):
            self.register(key, f, scope)
            return f

        return deco

    # ---- 解決 ----

    def depends(self, key):
        """The ``veil.Depends`` marker for ``key`` (``Inject(key)`` uses the current container)."""
        reg = self._regs.get(key)
        scope = reg.scope if reg is not None else SCOPED
        return veil.Depends(self._resolver(key), use_cache=scope != TRANSIENT)

    def _resolver(self, key):
        stub = self._stubs.get(key)
        if stub is not None:
            return stub
        reg = self._regs.get(key)
        if reg is None:
            # まだ登録されていない: 呼ばれたとき（起動後）に解決する（ジェネレータの提供者は要求の後に閉じる）
            container = self

            def late(request: veil.Request):
                o = container._overrides.get(key, _MISSING)
                if o is not _MISSING:
                    yield o
                    return
                reg = container._regs.get(key)
                if reg is None:
                    raise LookupError("no provider registered for %r" % (key,))
                if reg.has_value or reg.scope == SINGLETON:
                    yield container.get(key)
                    return
                v = reg.factory(**container._registered_kwargs(reg.factory))
                if inspect.isgenerator(v):
                    yield from _once(v)
                    return
                if inspect.isawaitable(v):
                    from veil import _run_coro

                    v = _run_coro(v)
                yield v

            late._veil_inline = True
            stub = late
        else:
            stub = self._make_resolver(reg)
        self._stubs[key] = stub
        return stub

    def _make_resolver(self, reg):
        container = self
        key = reg.key
        if reg.has_value:
            def value_resolver():
                o = container._overrides.get(key, _MISSING)
                return reg.value if o is _MISSING else o

            value_resolver._veil_inline = True
            return value_resolver
        factory = reg.factory
        sig = _injected_signature(self, factory)
        regs = self._regs
        kind = "gen" if inspect.isgeneratorfunction(factory) else (
            "agen" if inspect.isasyncgenfunction(factory) else (
                "async" if inspect.iscoroutinefunction(factory) else "sync"))

        if reg.scope == SINGLETON:
            def resolver(**kw):
                o = container._overrides.get(key, _MISSING)
                if o is not _MISSING:
                    return o
                cur = regs[key]
                if not cur.has_value:
                    with cur.lock:
                        if not cur.has_value:
                            v = cur.factory(**kw)
                            if inspect.isawaitable(v):
                                from veil import _run_coro

                                v = _run_coro(v)
                            cur.value, cur.has_value = v, True
                return cur.value
        elif kind == "gen":
            def resolver(**kw):
                o = container._overrides.get(key, _MISSING)
                if o is not _MISSING:
                    yield o
                    return
                yield from regs[key].factory(**kw)
        elif kind == "agen":
            async def resolver(**kw):
                o = container._overrides.get(key, _MISSING)
                if o is not _MISSING:
                    yield o
                    return
                async for v in regs[key].factory(**kw):
                    yield v
        elif kind == "async":
            async def resolver(**kw):
                o = container._overrides.get(key, _MISSING)
                if o is not _MISSING:
                    return o
                return await regs[key].factory(**kw)
        else:
            def resolver(**kw):
                o = container._overrides.get(key, _MISSING)
                if o is not _MISSING:
                    return o
                return regs[key].factory(**kw)

        resolver.__signature__ = sig
        resolver.__annotations__ = {p.name: p.annotation for p in sig.parameters.values()
                                    if p.annotation is not inspect.Parameter.empty}
        resolver.__name__ = "provide_%s" % getattr(key, "__name__", "value")
        resolver.__qualname__ = resolver.__name__
        resolver.__module__ = getattr(factory, "__module__", __name__)
        if getattr(factory, "_veil_inline", False):
            resolver._veil_inline = True
        return resolver

    def get(self, key):
        """Resolve ``key`` outside a request (singletons and providers without request parameters; a generator
        provider gives its first value and its code after ``yield`` is not run)."""
        o = self._overrides.get(key, _MISSING)
        if o is not _MISSING:
            return o
        reg = self._regs.get(key)
        if reg is None:
            raise LookupError("no provider registered for %r" % (key,))
        if reg.has_value:
            return reg.value
        kwargs = self._registered_kwargs(reg.factory)
        if reg.scope == SINGLETON:
            with reg.lock:
                if not reg.has_value:
                    reg.value, reg.has_value = reg.factory(**kwargs), True
            return reg.value
        v = reg.factory(**kwargs)
        if inspect.isgenerator(v):
            # 要求の外: 最初の値を返す（yield の後は実行しない）
            return next(v)
        if inspect.isawaitable(v):
            from veil import _run_coro

            v = _run_coro(v)
        return v

    def _registered_kwargs(self, factory):
        # 登録された型の引数だけを解決する（要求の引数は無い前提）
        kwargs = {}
        for p in _injected_signature(self, factory).parameters.values():
            d = p.default
            if isinstance(d, veil.Depends) and d.dependency in self._stubs.values():
                kwargs[p.name] = self.get(_key_of(self, d.dependency))
        return kwargs

    # ---- 差し替え ----

    @contextlib.contextmanager
    def override(self, key, value):
        """Use ``value`` for ``key`` inside the ``with`` block (tests); requests in progress may see either."""
        old = self._overrides.get(key, _MISSING)
        self._overrides[key] = value
        try:
            yield value
        finally:
            if old is _MISSING:
                self._overrides.pop(key, None)
            else:
                self._overrides[key] = old

    def replace(self, key, factory, app=None):
        """Replace the provider of ``key`` while the server runs (new requests use it).

        A provider with the same parameters is swapped in place. One with other parameters needs ``app``: the routes
        are rebuilt through ``app.dependency_overrides``.
        """
        reg = self._regs.get(key)
        if reg is None:
            self.register(key, factory)
            return
        old = self._stubs.get(key)
        if app is not None and old is not None:
            new = self._make_resolver(_Registration(key, factory, reg.scope))
            app.dependency_overrides[old] = new
            return
        if old is not None and _injected_signature(self, factory) != _injected_signature(self, reg.factory):
            raise ValueError("the new provider has other parameters; pass app= to rebuild the routes")
        reg.factory = factory
        reg.has_value = False
        reg.value = None

    def reset(self, key):
        """Forget the cached singleton value of ``key``."""
        reg = self._regs.get(key)
        if reg is not None and reg.factory is not None:
            reg.has_value = False
            reg.value = None


_MISSING = object()


def _once(gen):
    # ジェネレータの提供者の最初の値を渡し、要求の後に残りを実行する
    v = next(gen)
    try:
        yield v
    finally:
        try:
            next(gen)
        except StopIteration:
            pass


def _key_of(container, resolver):
    for k, v in container._stubs.items():
        if v is resolver:
            return k
    raise LookupError(resolver)


def _injected_signature(container, factory):
    """The signature of ``factory`` with the parameters of registered types turned into ``Depends``."""
    target = factory.__init__ if inspect.isclass(factory) else factory
    try:
        sig = inspect.signature(factory)
    except (TypeError, ValueError):
        return inspect.Signature()
    try:
        import typing

        hints = typing.get_type_hints(target)
    except Exception:  # noqa: BLE001
        hints = {}
    params = []
    for p in sig.parameters.values():
        ann = hints.get(p.name, p.annotation)
        if p.default is inspect.Parameter.empty and ann in container._regs:
            p = p.replace(default=container.depends(ann), annotation=ann)
        elif ann is not inspect.Parameter.empty:
            p = p.replace(annotation=ann)
        params.append(p)
    return sig.replace(parameters=params, return_annotation=inspect.Signature.empty)


def Inject(key, container=None):  # noqa: N802
    """``param: T = Inject(T)``: the ``veil.Depends`` marker for ``key`` in ``container`` (the last created by default)."""
    c = container or _current
    if c is None:
        raise RuntimeError("create a Container before using Inject()")
    return c.depends(key)
