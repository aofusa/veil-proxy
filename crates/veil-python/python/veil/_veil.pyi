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
    "register_grpc_health",
    "register_l4_tcp",
    "register_l4_udp",
    "register_route",
    "run",
    "shutdown",
    "_json_fast",
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
    def to_toml(self) -> str:
        """Return the merged configuration as a TOML string (for debugging)."""

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
    def stream(self) -> NativeBodyIter:
        """Iterate over the body chunks as they arrive (``stream_body=True`` routes)."""
    def json(self) -> Any:
        """Parse the body as JSON."""
    def text(self) -> str:
        """Body decoded as UTF-8 (lossy)."""

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

def register_grpc(path: str, handler_id: int) -> None:
    """Register a gRPC method; ``handlers[handler_id].handle(call)`` returns ``(code, message)``."""

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
) -> None:
    """Register an HTTP route (collected until ``run``)."""

def run(config: Config, handlers: list[Any]) -> None:
    """Start the server and block until it stops (the GIL is released).

    ``handlers`` maps ``handler_id`` to the Python object registered for it.
    """

def shutdown() -> None:
    """Stop the running server gracefully."""

def _json_fast(v: Any) -> bytes | None:
    """Test helper: the Rust JSON encoding of ``v``, or ``None`` if unsupported."""
