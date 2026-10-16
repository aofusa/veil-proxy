# WebSocket and long-lived connections

The Python API of veil does **not** provide WebSocket endpoints. The veil reverse proxy itself can proxy
WebSocket upgrades to a backend (see the repository guide, `docs/guide/protocols.md`), but a Python handler
cannot terminate a WebSocket.

For server-to-client push and other long-lived exchanges use what the library offers:

- **Server-Sent Events**: return `veil.EventSourceResponse(iterable)` (see {doc}`requests-responses`). GraphQL
  subscriptions can be delivered over SSE; the repository example `examples/api/python-graphql/` does this
  with strawberry.
- **Streaming responses and request bodies**: generators, `veil.StreamingResponse` and `stream_body=True`.
- **Raw TCP and UDP (L4)**: `@veil.l4_tcp(name, listen)` on a `veil.Protocol` subclass (`connection_made`,
  `data_received`, `connection_lost`, like `asyncio.Protocol`; `self.transport.write` inside a callback) and
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
