"""Static files with the arguments of Starlette's ``StaticFiles``.

.. code-block:: python

    from veil_kit.staticfiles import StaticFiles

    app.mount("/static", StaticFiles(directory="public"), name="static")

The directory is served by the server's file backend (Rust): ``sendfile``, kTLS, Range, ``ETag`` and precompressed
files work as for the proxy, and requests do not reach Python. ``html=True`` serves ``index.html`` for directories.
"""

# F-268: 本体の File バックエンドのルートを注入する（app.serve_dir と同じ）

import os

__all__ = ["StaticFiles"]


class StaticFiles:
    """A directory to mount with ``app.mount(path, StaticFiles(directory=...))``."""

    def __init__(self, *, directory, html=False, check_dir=True, follow_symlink=False, cache=True, **route_options):
        """``directory`` is served below the mount path; ``cache=False`` turns off the open-file and content caches;
        ``route_options`` are extra ``[[route]]`` keys."""
        if check_dir and not os.path.isdir(directory):
            raise RuntimeError("Directory '%s' does not exist" % directory)
        self.directory = os.path.abspath(directory)
        self.html = html
        self.cache = cache
        self.route_options = route_options

    def _veil_mount(self, app, path):
        """Called by ``app.mount``."""
        app.serve_dir(path, self.directory, index="index.html" if self.html else None, cache=self.cache,
                      **self.route_options)
