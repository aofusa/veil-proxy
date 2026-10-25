# WebSocket and protocol upgrades

veil terminates WebSocket (RFC 6455) for Python handlers. The API has the names of Starlette / FastAPI:

```python
import veil

app = veil.App()

@app.websocket("/ws")
async def ws(websocket: veil.WebSocket):
    await websocket.accept()                     # subprotocol=..., headers=...
    async for text in websocket.iter_text():
        await websocket.send_text(text)
```

- The handler is an `async def` and runs on the persistent event loop (see {doc}`async`). Path and query
  parameters are bound by name and annotation as for HTTP routes (a validation error answers the handshake with 422).
- **Handshake.** Until `await websocket.accept()` the handshake is pending; returning without accepting answers 403,
  an exception before accepting 500 (as in Starlette). `websocket.close()` before accepting also rejects with 403.
- **Receiving:** `receive_text()`, `receive_bytes()`, `receive_json(mode="text")`, `receive()` (an ASGI-style dict:
  `{"type": "websocket.receive", "text" | "bytes": ...}` or `{"type": "websocket.disconnect", "code": ...}`), and
  `iter_text()`, `iter_bytes()`, `iter_json()` (they stop when the client closes). `receive_*` raises
  `veil.WebSocketDisconnect(code)` when the client closed (1000, 1001, ...; 1006 when it left without a Close).
- **Sending:** `send_text()`, `send_bytes()`, `send_json(data, mode="text")`, `send({"type": "websocket.send", ...})`
  and `close(code=1000, reason=None)`. Sending queues the message and returns; the network worker writes queued
  messages together (one `writev`). Sending to a closed connection raises `WebSocketDisconnect`.
- **Request information:** `headers`, `query_params`, `path_params`, `url`, `client` (`(host, None)`),
  `subprotocols` (offered by the client) and `state` (free-form attributes).
- **What veil does in Rust:** the handshake checks, frames, fragmentation, Ping / Pong, UTF-8 checks, close
  handshake and size limits (`[api] websocket_max_message_size`, 16 MiB by default; larger messages close with
  1009). Python only sees complete messages. When the handler falls behind, veil stops reading from the socket
  (TCP backpressure). Graceful shutdown closes open WebSockets with 1001.
- WebSocket works over HTTP/1.1 only (an HTTP/2 or HTTP/3 handshake is answered with 400).

A broadcast, as in the FastAPI documentation, works unchanged:

```python
class ConnectionManager:
    def __init__(self):
        self.active = []

    async def connect(self, websocket):
        await websocket.accept()
        self.active.append(websocket)

    async def broadcast(self, message):
        for c in self.active:
            await c.send_text(message)

manager = ConnectionManager()

@app.websocket("/chat")
async def chat(websocket: veil.WebSocket):
    await manager.connect(websocket)
    try:
        while True:
            await manager.broadcast(await websocket.receive_text())
    except veil.WebSocketDisconnect:
        manager.active.remove(websocket)
```

## OpenAPI

Unlike FastAPI, veil lists WebSocket routes in `/openapi.json`: a `GET` operation answered with 101 (and 400 /
426 for bad handshakes), the path and query parameters, `Sec-WebSocket-Protocol` and an `x-veil-websocket`
extension with the message schemas. The Swagger UI shows them without "Try it out".

```python
class ChatIn(pydantic.BaseModel):
    room: str
    text: str

@app.websocket("/chat", receive=ChatIn, send=ChatOut, subprotocols=["chat.v1"], tags=["chat"],
               summary="Chat", validate=True)
async def chat(websocket: veil.WebSocket):
    ...
```

`receive` (client to server) and `send` (server to client) are pydantic models. With `validate=True`,
`receive_json()` checks each message against `receive` and closes with 1007 when it does not match.
`include_in_schema=False` leaves the route out. `[api] openapi_websocket = false` leaves all WebSocket routes out.

## Class-based endpoints

```python
@app.websocket("/echo")
class Echo(veil.WebSocketEndpoint):
    encoding = "text"                       # "text", "bytes", "json" or None

    def on_connect(self, websocket):
        websocket.send_text("welcome")

    def on_receive(self, websocket, data):
        websocket.send_text(data)

    def on_disconnect(self, websocket, close_code):
        pass
```

The method names are Starlette's. The connection is accepted before `on_connect`. Plain `def` callbacks run on
the handler pool, one at a time per connection (blocking code is fine there); `async def` callbacks run on the
event loop. A message of the wrong kind for `encoding` closes with 1003.

## Other protocol upgrades

`veil.Response.switching_protocols(protocol, factory)` answers `101 Switching Protocols` to a request that asks for
`Upgrade: <protocol>` and serves the connection with the object `factory()` returns: a `veil.Protocol`
(`connection_made` / `data_received` / `connection_lost` on the handler pool, like an L4 TCP listener) or a
`veil.AsyncProtocol` whose callbacks may be `async def` (run on the event loop):

```python
class LineEcho(veil.AsyncProtocol):
    def connection_made(self, transport):
        self.transport = transport

    async def data_received(self, data):
        self.transport.write(data)

@app.get("/echo")
def echo(req: veil.Request):
    return veil.Response.switching_protocols("x-echo", LineEcho)
```

A request that does not ask for an upgrade gets 400 instead.

## Other long-lived exchanges

- **Server-Sent Events**: return `veil.EventSourceResponse(iterable)` (see {doc}`requests-responses`).
- **Streaming responses and request bodies**: generators, `veil.StreamingResponse` and `stream_body=True`.
- **Raw TCP and UDP (L4)**: `@veil.l4_tcp(name, listen)` on a `veil.Protocol` subclass and
  `@veil.l4_udp(name, listen)` on `def f(data, peer) -> bytes | None`:

```python
@veil.l4_tcp("echo", "127.0.0.1:7000")
class Echo(veil.Protocol):
    def data_received(self, data):
        self.transport.write(data)         # only inside a callback

@veil.l4_udp("udp_echo", "127.0.0.1:7001")
def udp_echo(data, peer):                  # peer = (host, port); a returned bytes is the reply
    return data
```

The callbacks run on the handler pool, one at a time per connection.
