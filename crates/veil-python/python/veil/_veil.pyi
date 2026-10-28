"""Type stubs for the native extension ``veil._veil`` (PyO3).

Applications use the ``veil`` package; this module is the low-level layer that
``veil`` drives (route registration, ``run``) and the Rust-backed objects the
server passes to handlers (``NativeRequest`` and friends).
"""

from typing import Any, Iterator, final

__all__ = [
    "Config",
    "NativeRequest",
    "NativeHeaders",
    "NativeBodyIter",
    "NativeGrpcCall",
    "NativeTransport",
    "register_grpc",
    "register_grpc_descriptors",
    "register_grpc_health",
    "register_l4_tcp",
    "register_l4_udp",
    "register_route",
    "run",
    "shutdown",
    "_json_fast",
    "parse_multipart",
    "spool_threshold",
    "AioJob",
    "AioStream",
    "NativeWebSocket",
    "NativeWsSender",
    "aio_setup",
    "aio_take",
    "aio_teardown",
    "register_websocket",
    "register_asgi",
    "register_cors",
    "register_static",
    "grpc_client_setup",
    "GrpcChannel",
    "GrpcCall",
]

@final
class Config:
    """Server configuration (a wrapper of ``veil::api::Config``)."""

    def __init__(self) -> None:
        """Create the library default configuration."""
    @staticmethod
    def from_file(path: str) -> Config:
        """Load a ``config.toml``."""
    def set(self, path: str, toml_value: str) -> None:
        """Override any key with a TOML literal, e.g. ``set("server.threads", "4")``."""
    def listen(self, addr: str) -> None:
        """Set ``[server].listen`` (TLS listener; use together with ``tls``)."""
    def listen_cleartext(self, addr: str) -> None:
        """Set ``[server].listen_cleartext`` (HTTP/1.1 + h2c; default ``127.0.0.1:8080``)."""
    def tls(self, cert_path: str, key_path: str) -> None:
        """Set ``[tls].cert_path`` and ``key_path``."""
    def threads(self, n: int) -> None:
        """Set ``[server].threads``."""
    def add_listener(
        self,
        listen: str,
        tls: bool = False,
        *,
        name: str | None = None,
        http2: bool | None = None,
        http3: bool = False,
        http3_listen: str | None = None,
        allow_http: bool | None = None,
        allow_h2c: bool | None = None,
        secure_redirect: bool | None = None,
        redirect_to: str | None = None,
        threads: int | None = None,
        unix_socket_permissions: str | None = None,
    ) -> None:
        """Add a ``[[listener]]`` (one port); see :class:`veil.Listener`."""
    def to_toml(self) -> str:
        """Return the merged configuration as a TOML string (for debugging)."""
    def api_setting(self, name: str) -> Any:
        """The value of ``[api].<name>`` in the merged configuration, or ``None``."""

@final
class NativeHeaders:
    """Case-insensitive read-only request headers (same shape as ``veil.Headers``)."""

    def get(self, name: str, default: Any = None) -> Any:
        """First value of header ``name`` (case-insensitive), or ``default``."""
    def getlist(self, name: str) -> list[str]:
        """All values of header ``name``, in arrival order."""
    def __getitem__(self, name: str, /) -> str:
        """First value of header ``name``; raises ``KeyError`` if absent."""
    def __contains__(self, name: str, /) -> bool:
        """True if header ``name`` is present."""
    def __len__(self) -> int:
        """Number of header lines (duplicates counted)."""
    def __iter__(self) -> Iterator[str]:
        """Iterate over distinct header names."""
    def keys(self) -> list[str]:
        """Distinct header names (first spelling seen)."""
    def items(self) -> list[tuple[str, str]]:
        """All ``(name, value)`` pairs in arrival order."""

@final
class NativeBodyIter:
    """Iterator returned by ``NativeRequest.stream()``."""

    def __iter__(self) -> NativeBodyIter:
        """Return the iterator itself."""
    def __next__(self) -> bytes:
        """Next body chunk."""

@final
class NativeRequest:
    """The request object passed to handlers (same shape as ``veil.Request``)."""

    def __new__(
        cls,
        method: str = "GET",
        path: str = "/",
        query: str = "",
        headers: list[tuple[str, str]] | None = None,
        body: bytes | None = None,
        params: dict[str, str] | None = None,
        client_ip: str = "",
        tls: bool = False,
    ) -> NativeRequest:
        """Build a request directly (tests)."""
    @property
    def method(self) -> str:
        """HTTP method, upper-case."""
    @property
    def path(self) -> str:
        """Request path without the query string."""
    @property
    def query(self) -> str:
        """Raw query string without the leading ``?``."""
    @property
    def body(self) -> bytes:
        """Whole request body."""
    @property
    def client_ip(self) -> str:
        """Client IP address."""
    @property
    def params(self) -> dict[str, str]:
        """Path parameters."""
    @property
    def headers(self) -> NativeHeaders:
        """Request headers."""
    @property
    def query_params(self) -> dict[str, str]:
        """Query parameters (the first value of a repeated key)."""
    @property
    def cookies(self) -> dict[str, str]:
        """The ``Cookie`` header as a dict (the last value of a repeated name)."""
    @property
    def http_version(self) -> str:
        """``"1.1"``, ``"2"`` or ``"3"``."""
    @property
    def is_tls(self) -> bool:
        """Whether veil received the request over TLS (HTTP/3 always)."""
    def stream(self) -> NativeBodyIter:
        """Iterate over the body chunks as they arrive (``stream_body=True`` routes)."""
    def json(self) -> Any:
        """Parse the body as JSON."""
    def text(self) -> str:
        """Body decoded as UTF-8 (lossy)."""
    def multipart(
        self,
    ) -> list[tuple[str | None, str | None, str | None, list[tuple[str, str]], bytes | None, int | None, int]]:
        """Parts of a ``multipart/form-data`` body: ``(name, filename, content_type, headers, data, fd, size)``.

        ``data`` is ``None`` when the file was written to the temporary file descriptor ``fd``. Malformed
        bodies raise ``ValueError((status, message))``.
        """

@final
class NativeGrpcCall:
    """One gRPC call; valid only while the handler runs."""

    def recv(self) -> bytes | None:
        """Next request message, or ``None`` at the end of the stream."""
    def send(self, data: bytes) -> bool:
        """Send a response message. ``False`` if the client has gone."""
    def add_header(self, name: str, value: str | bytes) -> bool:
        """Add a response header (before the first ``send``; otherwise ``False``)."""
    def add_trailer(self, name: str, value: str | bytes) -> None:
        """Add a response trailer."""
    def metadata(self) -> list[tuple[str, str | bytes]]:
        """Request metadata (``-bin`` values are ``bytes``)."""
    def method(self) -> str:
        """Full method path, e.g. ``/pkg.Svc/Method``."""
    def time_remaining(self) -> float | None:
        """Seconds until the deadline, or ``None`` if there is none."""
    def is_active(self) -> bool:
        """``False`` once the client has cancelled or disconnected."""

@final
class NativeTransport:
    """Send side of an L4 TCP connection (asyncio-transport-like)."""

    def write(self, data: bytes) -> None:
        """Send bytes (valid only inside the protocol callbacks)."""
    def close(self) -> None:
        """Close after the pending data has been sent."""
    def is_closing(self) -> bool:
        """``True`` after ``close()``."""
    def get_extra_info(self, name: str, default: Any = None) -> Any:
        """``"peername"`` gives ``(host, port)``; other names give ``default``."""

@final
class AioJob:
    """One unit of work for a persistent event loop (a call, a cancellation, a WebSocket or a wake-up)."""

    kind: int
    id: int
    handler_id: int
    @property
    def request(self) -> NativeRequest | None:
        """The request of a call or WebSocket job."""
    @property
    def websocket(self) -> NativeWebSocket | None:
        """The connection of a WebSocket job."""
    def complete(self, result: Any) -> None:
        """Answer the call with a handler return value."""
    def fail(self) -> None:
        """Answer the call with 500."""
    def cancelled(self) -> None:
        """Acknowledge that the call was cancelled."""
    def start_stream(self, sr: Any) -> AioStream:
        """Answer the call with a streaming response and return its writer."""

@final
class AioStream:
    """Writer of a streaming response produced on the event loop."""

    def try_write(self, item: bytes | str) -> bool:
        """Queue a chunk; ``False`` when the queue is full (retry later)."""
    @property
    def closed(self) -> bool:
        """``True`` once the client disconnected."""
    def finish(self, trailers: Any = None) -> None:
        """End the response (with optional trailers)."""

@final
class NativeWebSocket:
    """Native side of one WebSocket connection (see :class:`veil.WebSocket`)."""

    def accept(self, subprotocol: str | None = None, headers: list[tuple[str, str]] = ...) -> None:
        """Answer the handshake with 101."""
    def reject(self, status: int) -> None:
        """Answer the handshake with an error status."""
    def take(self) -> tuple[str, Any] | None:
        """The next received message (``("text", str)``, ``("bytes", bytes)``, ``("close", code)``) or ``None``."""
    def send(self, opcode: int, data: bytes | str) -> int:
        """Queue a message: 0 = queued, 1 = queue full, 2 = closed."""
    def close(self, code: int, reason: str) -> None:
        """Start the closing handshake."""
    def finish(self) -> None:
        """The handler returned (closes the connection when still open)."""

@final
class NativeWsSender:
    """Send side of a :class:`veil.WebSocketEndpoint` connection."""

    def send(self, opcode: int, data: bytes | str) -> int:
        """Queue a message: 0 = queued, 1 = queue full, 2 = closed."""
    def close(self, code: int, reason: str) -> None:
        """Start the closing handshake."""

def aio_setup(fds: list[int], queue: int) -> None:
    """Register the wake-up sockets of the persistent event loops (one per loop) and the queue size."""

def aio_teardown() -> None:
    """Unregister the event loops (later jobs are refused, queued ones are settled)."""

def aio_take(index: int) -> list[AioJob]:
    """Take the queued jobs of event loop ``index``."""

def register_websocket(
    path: str,
    handler_id: int,
    openapi_json: str | None = None,
    endpoint: bool = False,
    hidden: bool = False,
) -> None:
    """Register an ``@app.websocket`` route (collected until ``run``)."""

def register_grpc(
    path: str, handler_id: int, request_streaming: bool = False, response_streaming: bool = False
) -> None:
    """Register a gRPC method; ``handlers[handler_id].handle(call)`` returns ``(code, message)``."""

def register_grpc_descriptors(descriptor_set: bytes) -> None:
    """Use a serialized ``FileDescriptorSet`` for the types of the gRPC methods in OpenAPI."""

def register_grpc_health() -> None:
    """Register ``grpc.health.v1.Health`` (Check and Watch)."""

def register_l4_tcp(name: str, listen: str, handler_id: int) -> None:
    """Register an L4 TCP listener; ``handlers[handler_id]`` creates the protocol."""

def register_l4_udp(name: str, listen: str, handler_id: int) -> None:
    """Register an L4 UDP listener; ``handlers[handler_id](data, peer)`` returns ``bytes | None``."""

def register_route(
    path: str,
    methods: list[str],
    handler_id: int,
    offload: bool = True,
    openapi_json: str | None = None,
    status_code: int = 200,
    stream_body: bool = False,
    on_loop: bool = False,
    form: bool = False,
    compress: bool = True,
    hidden: bool = False,
) -> None:
    """Register an HTTP route (collected until ``run``; ``on_loop`` runs it on the persistent event loop; ``form``
    reads the body first and splits a ``multipart/form-data`` body into parts; ``compress=False`` keeps its
    responses uncompressed; ``hidden`` leaves it out of OpenAPI)."""

def register_asgi(prefix: str, handler_id: int) -> None:
    """Mount an ASGI application (``handlers[handler_id]`` is a ``veil.asgi.ASGIApp``) at ``prefix``."""

def register_cors(
    prefix: str = "/",
    origins: list[str] = ...,
    any_origin: bool = False,
    methods: list[str] | None = None,
    headers: list[str] = ...,
    any_header: bool = False,
    expose: list[str] = ...,
    credentials: bool = False,
    max_age: int | None = None,
) -> None:
    """Answer CORS for the routes below ``prefix`` in the server (``ValueError`` for invalid settings)."""

def register_static(
    prefix: str, dir: str, index: str | None = None, cache: bool = True, extra_toml: str | None = None
) -> None:
    """Serve the directory ``dir`` below ``prefix`` with the server's file backend."""

def grpc_client_setup() -> None:
    """Start the gRPC client thread now (otherwise it starts with the first call)."""

@final
class GrpcChannel:
    """A gRPC channel on veil's HTTP/2 client (used by ``veil_kit.grpc.Channel``)."""

    def __new__(
        cls,
        url: str,
        ca: list[str] | None = None,
        cert: tuple[str, str] | None = None,
        gzip: bool = False,
        max_message_size: int | None = None,
    ) -> GrpcChannel: ...
    def call(
        self, method: str, metadata: list[tuple[str, str | bytes]] = ..., timeout: float | None = None
    ) -> GrpcCall:
        """Start a call of ``method`` (``/package.Service/Method``)."""

@final
class GrpcCall:
    """One call started by :meth:`GrpcChannel.call`."""

    def send(self, message: bytes) -> bool:
        """Send one request message; ``False`` once the call has finished or sending was closed."""
    def done_sending(self) -> None:
        """Finish sending (half-close)."""
    def recv(self) -> bytes | None:
        """The next response message, or ``None`` at the end (waits with the GIL released)."""
    def recv_cb(self, callback: Any) -> None:
        """Call ``callback(bytes | None)`` with the next response message (from another thread)."""
    def status(self) -> tuple[int, str, list[tuple[str, Any]], list[tuple[str, Any]]]:
        """``(code, details, initial_metadata, trailing_metadata)``; waits for the end of the call."""
    def status_cb(self, callback: Any) -> None:
        """Call ``callback((code, details, initial, trailing))`` at the end of the call."""
    def initial_metadata(self) -> list[tuple[str, Any]]:
        """The response headers (empty until they arrived)."""
    def cancel(self) -> None:
        """Cancel the call."""

def run(config: Config, handlers: list[Any]) -> None:
    """Start the server and block until it stops (the GIL is released).

    ``handlers`` maps ``handler_id`` to the Python object registered for it.
    """

def shutdown() -> None:
    """Stop the running server gracefully."""

def _json_fast(v: Any) -> bytes | None:
    """Test helper: the Rust JSON encoding of ``v``, or ``None`` if unsupported."""

def parse_multipart(
    body: bytes, content_type: str
) -> list[tuple[str | None, str | None, str | None, list[tuple[str, str]], bytes | None, int | None, int]]:
    """Parse a buffered ``multipart/form-data`` body (same tuples as ``NativeRequest.multipart``)."""

def spool_threshold() -> int:
    """``[api] multipart_spool_threshold``: uploads up to this size stay in memory."""
