"""Solving the dependencies of a route (``Depends``, ``Security``, ``BackgroundTasks``, ``Response`` parameters).

The tree of a route is built from the signatures once and reused; a request walks it in order. Plain functions are
called directly on the handler pool and, from ``async def`` routes on the event loop, on the thread pool (as FastAPI
does). Functions marked ``_veil_inline = True`` (cheap ones such as the security schemes of ``veil_kit``) are always
called directly.
"""

# F-268: 依存の解決。木は署名から 1 回だけ作り、要求ごとは決まった順に呼ぶだけ。

import inspect

from .params import SecurityScopes

# 依存の木の節の種類
SYNC, GEN, ASYNC, AGEN = 0, 1, 2, 3

# 検証に失敗した依存（キャッシュの印）
_FAILED = object()


def _first(gen):
    # スレッドプールの future に StopIteration を載せると asyncio が TypeError にするので、ここで変換する
    try:
        return next(gen)
    except StopIteration:
        raise RuntimeError("the dependency generator did not yield") from None


def _close(gen, exc):
    # yield の後を実行する（終わりの StopIteration はここで止める）
    try:
        if exc is None:
            next(gen)
        else:
            gen.throw(exc)
    except StopIteration:
        pass


def _kind_of(call):
    target = call
    if not (inspect.isfunction(call) or inspect.ismethod(call)) and not inspect.isclass(call):
        target = getattr(call, "__call__", call)
    if inspect.isasyncgenfunction(target):
        return AGEN
    if inspect.isgeneratorfunction(target):
        return GEN
    if inspect.iscoroutinefunction(target):
        return ASYNC
    return SYNC


class Dependant:
    """One function of the tree: its parameters (sub-dependencies included) and how to call it."""

    __slots__ = ("call", "params", "kind", "use_cache", "scopes", "inline", "name")

    def __init__(self, call, params, use_cache=True, scopes=()):
        self.call = call
        self.params = params
        self.kind = _kind_of(call)
        self.use_cache = use_cache
        self.scopes = tuple(scopes)
        self.inline = bool(getattr(call, "_veil_inline", False))
        self.name = getattr(call, "__name__", type(call).__name__)


class Overrides(dict):
    """``app.dependency_overrides``: a dict that counts its changes (the route trees are rebuilt after one)."""

    __slots__ = ("version",)

    def __init__(self, *a, **kw):
        super().__init__(*a, **kw)
        self.version = 0

    def _bump(self):
        self.version += 1

    def __setitem__(self, k, v):
        super().__setitem__(k, v)
        self._bump()

    def __delitem__(self, k):
        super().__delitem__(k)
        self._bump()

    def clear(self):
        super().clear()
        self._bump()

    def pop(self, *a):
        r = super().pop(*a)
        self._bump()
        return r

    def popitem(self):
        r = super().popitem()
        self._bump()
        return r

    def setdefault(self, k, default=None):
        r = super().setdefault(k, default)
        self._bump()
        return r

    def update(self, *a, **kw):
        super().update(*a, **kw)
        self._bump()


class Ctx:
    """The state of one request: cached dependency values, ``yield`` dependencies to finish, background tasks and the
    sub-response of ``Response`` parameters."""

    __slots__ = ("cache", "exits", "background", "response", "errors", "state")

    def __init__(self):
        self.cache = {}
        self.exits = []
        self.background = None
        self.response = None
        self.errors = []
        self.state = {}

    def background_tasks(self):
        if self.background is None:
            from .params import BackgroundTasks

            self.background = BackgroundTasks()
        return self.background

    def sub_response(self):
        if self.response is None:
            from . import Response

            self.response = Response(b"", status=0)
        return self.response


# ===================== 同期（ハンドラ用プールのスレッド） =====================


def solve_sync(dep, req, ctx, bind_one):
    """Bind the parameters of ``dep`` (calling its sub-dependencies); validation errors go to ``ctx.errors``."""
    kwargs = {}
    for p in dep.params:
        k = p.kind
        if k == "depends":
            sub = p.sub
            key = (sub.call, sub.scopes)
            if sub.use_cache and key in ctx.cache:
                v = ctx.cache[key]
                if v is _FAILED:
                    continue
            else:
                ok, v = call_sync(sub, req, ctx, bind_one)
                if not ok:
                    # 失敗も覚える（同じ依存の検証の誤りを 2 度数えない）
                    if sub.use_cache:
                        ctx.cache[key] = _FAILED
                    continue
                if sub.use_cache:
                    ctx.cache[key] = v
            if not p.hidden:
                kwargs[p.name] = v
        elif k == "background":
            kwargs[p.name] = ctx.background_tasks()
        elif k == "response":
            kwargs[p.name] = ctx.sub_response()
        elif k == "scopes":
            kwargs[p.name] = SecurityScopes(dep.scopes)
        else:
            bind_one(p, req, kwargs, ctx.errors, ctx.state)
    return kwargs


def call_sync(dep, req, ctx, bind_one):
    from . import _run_coro

    n = len(ctx.errors)
    kwargs = solve_sync(dep, req, ctx, bind_one)
    if len(ctx.errors) != n:
        return False, None
    call = dep.call
    kind = dep.kind
    if kind == SYNC:
        return True, call(**kwargs)
    if kind == ASYNC:
        return True, _run_coro(call(**kwargs))
    if kind == GEN:
        gen = call(**kwargs)
        v = _first(gen)
        ctx.exits.append((gen, True))
        return True, v
    agen = call(**kwargs)
    v = _run_coro(agen.__anext__())
    ctx.exits.append((agen, True))
    return True, v


def finish_sync(ctx, exc):
    """Run the code after ``yield`` of the dependencies (last first). Returns the exception to raise, if any."""
    from . import _run_coro

    while ctx.exits:
        g, _ = ctx.exits.pop()
        try:
            if inspect.isasyncgen(g):
                if exc is None:
                    _run_coro(g.__anext__())
                else:
                    _run_coro(g.athrow(exc))
            elif exc is None:
                next(g)
            else:
                g.throw(exc)
        except (StopIteration, StopAsyncIteration):
            pass
        except BaseException as e:  # noqa: BLE001  後始末が別の例外（HTTPException 等）を投げたら差し替える
            exc = e
    return exc


# ===================== 非同期（常駐のイベントループ） =====================


async def solve_async(dep, req, ctx, bind_one):
    kwargs = {}
    for p in dep.params:
        k = p.kind
        if k == "depends":
            sub = p.sub
            key = (sub.call, sub.scopes)
            if sub.use_cache and key in ctx.cache:
                v = ctx.cache[key]
                if v is _FAILED:
                    continue
            else:
                ok, v = await call_async(sub, req, ctx, bind_one)
                if not ok:
                    # 失敗も覚える（同じ依存の検証の誤りを 2 度数えない）
                    if sub.use_cache:
                        ctx.cache[key] = _FAILED
                    continue
                if sub.use_cache:
                    ctx.cache[key] = v
            if not p.hidden:
                kwargs[p.name] = v
        elif k == "background":
            kwargs[p.name] = ctx.background_tasks()
        elif k == "response":
            kwargs[p.name] = ctx.sub_response()
        elif k == "scopes":
            kwargs[p.name] = SecurityScopes(dep.scopes)
        else:
            bind_one(p, req, kwargs, ctx.errors, ctx.state)
    return kwargs


async def call_async(dep, req, ctx, bind_one):
    from ._aio import run_in_threadpool

    n = len(ctx.errors)
    kwargs = await solve_async(dep, req, ctx, bind_one)
    if len(ctx.errors) != n:
        return False, None
    call = dep.call
    kind = dep.kind
    if kind == ASYNC:
        return True, await call(**kwargs)
    if kind == AGEN:
        agen = call(**kwargs)
        v = await agen.__anext__()
        ctx.exits.append((agen, True))
        return True, v
    if kind == GEN:
        gen = call(**kwargs)
        v = _first(gen) if dep.inline else await run_in_threadpool(_first, gen)
        ctx.exits.append((gen, dep.inline))
        return True, v
    if dep.inline:
        return True, call(**kwargs)
    return True, await run_in_threadpool(call, **kwargs)


async def finish_async(ctx, exc):
    from ._aio import run_in_threadpool

    while ctx.exits:
        g, inline = ctx.exits.pop()
        try:
            if inspect.isasyncgen(g):
                if exc is None:
                    await g.__anext__()
                else:
                    await g.athrow(exc)
            elif inline:
                _close(g, exc)
            else:
                await run_in_threadpool(_close, g, exc)
        except (StopIteration, StopAsyncIteration):
            pass
        except BaseException as e:  # noqa: BLE001
            exc = e
    return exc
