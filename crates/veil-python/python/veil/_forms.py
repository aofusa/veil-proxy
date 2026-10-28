"""Form fields and file uploads (``multipart/form-data`` and ``application/x-www-form-urlencoded``).

In the server the body is read in Rust before the handler runs: text fields and small files stay in
memory, files larger than ``[api] multipart_spool_threshold`` are written to anonymous temporary files
(``[api] upload_tmp_dir``) and handed over as file descriptors, so a large upload never sits in memory.
"""

# F-266

import io
import os
import re
import sys

_EMPTY = object()


class _FormMarker:
    __slots__ = ("default", "alias", "description", "file")

    def __init__(self, default, alias, description, file):
        self.default = default
        self.alias = alias
        self.description = description
        self.file = file

    def __repr__(self):
        return "%s(%r)" % ("File" if self.file else "Form", self.default)


def Form(default=..., *, alias=None, description=None):  # noqa: N802 (FastAPI と同じ名前)
    """Declare a form field (a default value, or ``...`` for a required field).

    ``name: str = Form()`` and ``name: Annotated[str, Form()]`` both work.
    """
    return _FormMarker(default, alias, description, False)


def File(default=..., *, alias=None, description=None):  # noqa: N802
    """Declare a file field of a ``multipart/form-data`` body.

    Annotate the parameter with :class:`UploadFile` (or ``list[UploadFile]``) to get the file object,
    or with ``bytes`` to get its content.
    """
    return _FormMarker(default, alias, description, True)


class UploadFile:
    """An uploaded file.

    ``file`` is a binary file object positioned at the start (in memory, or an anonymous temporary
    file that disappears when it is closed). The ``async`` methods match FastAPI's ``UploadFile``.
    """

    __slots__ = ("filename", "content_type", "headers", "size", "file")

    def __init__(self, file, filename=None, content_type=None, headers=None, size=None):
        self.file = file
        self.filename = filename
        self.content_type = content_type
        self.headers = dict(headers or ())
        self.size = size

    def __repr__(self):
        return "UploadFile(filename=%r, content_type=%r, size=%r)" % (self.filename, self.content_type, self.size)

    def read_bytes(self):
        """Return the whole content (from the start)."""
        self.file.seek(0)
        return self.file.read()

    async def read(self, size=-1):
        """Read up to ``size`` bytes (all by default)."""
        return self.file.read(size)

    async def write(self, data):
        """Write ``data`` at the current position."""
        return self.file.write(data)

    async def seek(self, offset):
        """Move to ``offset``."""
        return self.file.seek(offset)

    async def close(self):
        """Close the file (a temporary file is removed)."""
        self.file.close()


def _fd_file(fd):
    if sys.platform == "win32":
        import msvcrt

        fd = msvcrt.open_osfhandle(fd, os.O_RDONLY | getattr(os, "O_BINARY", 0))
    f = os.fdopen(fd, "rb")
    f.seek(0)
    return f


class Part:
    """One part of a form body (internal)."""

    __slots__ = ("name", "filename", "content_type", "headers", "data", "fd", "size")

    def __init__(self, name, filename, content_type, headers, data, fd, size):
        self.name = name
        self.filename = filename
        self.content_type = content_type
        self.headers = headers
        self.data = data
        self.fd = fd
        self.size = size

    def __del__(self):
        # 使われなかった一時ファイル（検証エラー・引数に無いパート）を閉じる
        fd, self.fd = self.fd, None
        if fd is not None:
            try:
                if sys.platform == "win32":
                    import _winapi

                    _winapi.CloseHandle(fd)
                else:
                    os.close(fd)
            except OSError:
                pass

    def is_file(self):
        return self.filename is not None

    def upload(self):
        f = _fd_file(self.fd) if self.fd is not None else io.BytesIO(self.data)
        self.fd = None
        return UploadFile(f, self.filename, self.content_type, self.headers, self.size)

    def content(self):
        if self.fd is not None:
            f = _fd_file(self.fd)
            self.fd = None
            with f:
                self.data = f.read()
        return self.data


class FormError(Exception):
    """A malformed or too large form body (``status``, ``message``)."""

    def __init__(self, status, message):
        super().__init__(status, message)
        self.status = status
        self.message = message


_BOUNDARY_RE = re.compile(r'boundary=(?:"([^"]+)"|([^;\s]+))', re.I)


def _py_parse(body, content_type):
    # 拡張無しの単体試験用（サーバでは Rust のパーサを使う）
    m = _BOUNDARY_RE.search(content_type)
    if not m:
        raise FormError(400, "expected a multipart/form-data body with a boundary")
    delim = b"--" + (m.group(1) or m.group(2)).encode("latin-1")
    chunks = body.split(delim)
    if len(chunks) < 3 or not chunks[-1].startswith(b"--"):
        raise FormError(400, "malformed multipart body")
    out = []
    for c in chunks[1:-1]:
        if not c.startswith(b"\r\n") or not c.endswith(b"\r\n"):
            raise FormError(400, "malformed multipart body")
        head, sep, data = c[2:-2].partition(b"\r\n\r\n")
        if not sep:
            head, data = c[2:-2].rstrip(b"\r\n"), b""
        headers = []
        name = filename = ctype = None
        for line in head.decode("utf-8", "replace").split("\r\n"):
            k, _, v = line.partition(":")
            k, v = k.strip().lower(), v.strip()
            headers.append((k, v))
            if k == "content-disposition":
                for pm in re.finditer(r';\s*([\w*]+)="?([^";]*)"?', v):
                    if pm.group(1).lower() == "name":
                        name = pm.group(2)
                    elif pm.group(1).lower() == "filename":
                        filename = pm.group(2)
            elif k == "content-type":
                ctype = v
        out.append(Part(name, filename, ctype, headers, data, None, len(data)))
    return out


def parts_of(req):
    """Return the parts of the request body (``multipart/form-data`` or urlencoded) as a list of :class:`Part`."""
    ctype = req.headers.get("content-type") or ""
    if ctype.split(";", 1)[0].strip().lower() == "multipart/form-data":
        try:
            if hasattr(req, "multipart"):
                raw = req.multipart()
            else:
                try:
                    from . import _veil

                    native = _veil.parse_multipart
                except (ImportError, AttributeError):
                    native = None
                if native is None:
                    return _py_parse(req.body, ctype)
                raw = native(req.body, ctype)
        except ValueError as e:
            # ネイティブの ValueError((status, message)) は args が (status, message) になる
            if len(e.args) == 2 and isinstance(e.args[0], int):
                raise FormError(*e.args)
            raise
        return [Part(*t) for t in raw]
    import urllib.parse

    body = req.body
    if not body:
        return []
    try:
        pairs = urllib.parse.parse_qsl(body.decode("utf-8"), keep_blank_values=True)
    except UnicodeDecodeError:
        raise FormError(400, "form body is not valid UTF-8")
    return [Part(k, None, None, [], v, None, len(v)) for k, v in pairs]
