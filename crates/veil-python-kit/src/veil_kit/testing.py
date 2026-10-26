"""Test helpers: the in-process :class:`TestClient` and dependency overrides as context managers."""

# F-268

import contextlib

from veil.testclient import TestClient, TestResponse

__all__ = ["TestClient", "TestResponse", "override_dependency"]


@contextlib.contextmanager
def override_dependency(app, dependency, replacement):
    """Use ``replacement`` for ``dependency`` (in ``app.dependency_overrides``) inside the ``with`` block."""
    had = dependency in app.dependency_overrides
    old = app.dependency_overrides.get(dependency)
    app.dependency_overrides[dependency] = replacement
    try:
        yield replacement
    finally:
        if had:
            app.dependency_overrides[dependency] = old
        else:
            del app.dependency_overrides[dependency]
