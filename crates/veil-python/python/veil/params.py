"""Parameter markers and dependency injection in the style of FastAPI.

``Query()``, ``Path()``, ``Header()``, ``Cookie()`` and ``Body()`` declare where an argument comes from and how it is
validated; ``Depends()`` and ``Security()`` declare dependencies. They are used as defaults
(``q: str = Query(max_length=20)``) or as ``Annotated`` metadata (``q: Annotated[str, Query(max_length=20)]``).

The dependency tree of a route is built once, when the route is first used (and again after
``app.dependency_overrides`` changes); a request only calls the functions in the stored order.
"""

# F-268: FastAPI の Depends・引数の印。依存の木は初回（と dependency_overrides の変更後）に 1 回だけ作る。

import inspect

__all__ = [
    "Query",
    "Path",
    "Header",
    "Cookie",
    "Body",
    "Depends",
    "Security",
    "SecurityScopes",
    "BackgroundTasks",
]


class _ParamMarker:
    """Where a parameter comes from (``query``, ``path``, ``header``, ``cookie`` or ``body``) and its rules."""

    __slots__ = (
        "where",
        "default",
        "alias",
        "rules",
        "description",
        "title",
        "examples",
        "deprecated",
        "include_in_schema",
        "convert_underscores",
        "embed",
        "media_type",
    )

    def __init__(self, where, default, alias, rules, description=None, title=None, examples=None,
                 deprecated=None, include_in_schema=True, convert_underscores=True, embed=False,
                 media_type="application/json"):
        self.where = where
        self.default = default
        self.alias = alias
        self.rules = rules
        self.description = description
        self.title = title
        self.examples = examples
        self.deprecated = deprecated
        self.include_in_schema = include_in_schema
        self.convert_underscores = convert_underscores
        self.embed = embed
        self.media_type = media_type

    def __repr__(self):
        return "%s(%r)" % (self.where.capitalize(), self.default)


def _rules(gt, ge, lt, le, min_length, max_length, pattern, regex):
    # JSON Schema の名前（body_constraints の検査と同じ規則で調べる）
    r = {}
    if ge is not None:
        r["minimum"] = ge
    if le is not None:
        r["maximum"] = le
    if gt is not None:
        r["exclusiveMinimum"] = gt
    if lt is not None:
        r["exclusiveMaximum"] = lt
    if min_length is not None:
        r["minLength"] = min_length
    if max_length is not None:
        r["maxLength"] = max_length
    if pattern is None:
        pattern = regex
    if pattern is not None:
        r["pattern"] = pattern
    return r


def Query(default=..., *, alias=None, title=None, description=None, gt=None, ge=None, lt=None, le=None,  # noqa: N802
          min_length=None, max_length=None, pattern=None, regex=None, examples=None, deprecated=None,
          include_in_schema=True):
    """A query parameter (``...`` or no default: required). ``list[str]`` collects repeated keys."""
    return _ParamMarker("query", default, alias, _rules(gt, ge, lt, le, min_length, max_length, pattern, regex),
                        description, title, examples, deprecated, include_in_schema)


def Path(default=..., *, alias=None, title=None, description=None, gt=None, ge=None, lt=None, le=None,  # noqa: N802
         min_length=None, max_length=None, pattern=None, regex=None, examples=None, deprecated=None,
         include_in_schema=True):
    """A path parameter with validation rules (path parameters are always required)."""
    return _ParamMarker("path", default, alias, _rules(gt, ge, lt, le, min_length, max_length, pattern, regex),
                        description, title, examples, deprecated, include_in_schema)


def Header(default=..., *, alias=None, convert_underscores=True, title=None, description=None, gt=None,  # noqa: N802
           ge=None, lt=None, le=None, min_length=None, max_length=None, pattern=None, regex=None, examples=None,
           deprecated=None, include_in_schema=True):
    """A request header. The name is the parameter name with ``_`` replaced by ``-`` (``convert_underscores``)."""
    return _ParamMarker("header", default, alias, _rules(gt, ge, lt, le, min_length, max_length, pattern, regex),
                        description, title, examples, deprecated, include_in_schema, convert_underscores)


def Cookie(default=..., *, alias=None, title=None, description=None, gt=None, ge=None, lt=None, le=None,  # noqa: N802
           min_length=None, max_length=None, pattern=None, regex=None, examples=None, deprecated=None,
           include_in_schema=True):
    """A cookie of the request."""
    return _ParamMarker("cookie", default, alias, _rules(gt, ge, lt, le, min_length, max_length, pattern, regex),
                        description, title, examples, deprecated, include_in_schema)


def Body(default=..., *, embed=False, media_type="application/json", alias=None, title=None,  # noqa: N802
         description=None, gt=None, ge=None, lt=None, le=None, min_length=None, max_length=None, pattern=None,
         regex=None, examples=None, deprecated=None, include_in_schema=True):
    """A JSON body (or one key of it with ``embed=True``; several ``Body`` parameters are always embedded)."""
    return _ParamMarker("body", default, alias, _rules(gt, ge, lt, le, min_length, max_length, pattern, regex),
                        description, title, examples, deprecated, include_in_schema, embed=embed,
                        media_type=media_type)


class Depends:
    """A dependency: ``dependency`` is called with its own parameters bound like a handler's, and its result is
    passed to the parameter. Without ``dependency`` the annotation of the parameter is called.

    A function using ``yield`` runs its code after ``yield`` when the handler has returned (before the response is
    sent; an exception of the handler is raised at the ``yield``). With ``use_cache=True`` (the default) a
    dependency used several times in one request is called once.
    """

    __slots__ = ("dependency", "use_cache", "scopes")

    def __init__(self, dependency=None, *, use_cache=True):
        """Declare a dependency on ``dependency``."""
        self.dependency = dependency
        self.use_cache = use_cache
        self.scopes = ()

    def __repr__(self):
        name = getattr(self.dependency, "__name__", type(self.dependency).__name__)
        return "%s(%s)" % (type(self).__name__, name)


class Security(Depends):
    """A dependency that also requires OAuth2 ``scopes``; they are added to the :class:`SecurityScopes` of the
    dependencies below it and to the security requirements in OpenAPI."""

    __slots__ = ()

    def __init__(self, dependency=None, *, scopes=None, use_cache=True):
        """Declare a security dependency on ``dependency`` with ``scopes``."""
        super().__init__(dependency, use_cache=use_cache)
        self.scopes = tuple(scopes or ())


class SecurityScopes:
    """The scopes required by the :class:`Security` dependencies above a dependency (declare a parameter of this
    type to get it)."""

    __slots__ = ("scopes",)

    def __init__(self, scopes=None):
        """Wrap a list of scopes."""
        self.scopes = list(scopes or [])

    @property
    def scope_str(self):
        """The scopes separated by spaces (for ``WWW-Authenticate``)."""
        return " ".join(self.scopes)

    def __repr__(self):
        return "SecurityScopes(%r)" % (self.scopes,)


class BackgroundTasks:
    """Functions to run after the response has been sent (declare a parameter of this type to get the request's).

    Plain functions run on the handler pool, ``async def`` functions on the event loop. A failing task is logged and
    does not stop the others.
    """

    __slots__ = ("tasks",)

    def __init__(self, tasks=None):
        """Create an empty list of tasks."""
        self.tasks = list(tasks or [])

    def add_task(self, func, *args, **kwargs):
        """Run ``func(*args, **kwargs)`` after the response."""
        self.tasks.append((func, args, kwargs))

    def __bool__(self):
        return bool(self.tasks)

    def __call__(self):
        """Run the tasks in order (called by the server once the response was sent)."""
        from . import _run_background

        _run_background(self.tasks)


def _is_marker(v):
    return isinstance(v, (_ParamMarker, Depends))


def _dependency_call(marker, ann):
    call = marker.dependency
    if call is None:
        if ann is inspect.Parameter.empty or not callable(ann):
            raise TypeError("Depends() without a dependency needs a callable annotation")
        call = ann
    return call
