"""veil の Python の F-210 の機能: ストリーミング応答（ジェネレータ・SSE・非同期ジェネレータ）・
要求本文のストリーミング受信・L4（TCP・UDP のエコー）。

    VEIL_LISTEN=127.0.0.1:8000 VEIL_L4_TCP=127.0.0.1:7000 VEIL_L4_UDP=127.0.0.1:7001 python app.py
"""

import asyncio
import os
import time

import veil


@veil.get("/stream")
def stream():
    # ジェネレータを返すと断片ごとに送る（text/plain）
    for i in range(1, 4):
        yield "line %d\n" % i
        time.sleep(0.01)


@veil.get("/sse")
def sse():
    def events():
        for i in range(1, 4):
            yield {"event": "tick", "data": {"n": i}}

    return veil.EventSourceResponse(events())


@veil.get("/async-stream")
async def async_stream():
    async def gen():
        for i in range(3):
            await asyncio.sleep(0.01)
            yield b"a%d," % i

    return veil.StreamingResponse(gen(), media_type="text/plain", trailers=[("x-parts", "3")])


@veil.post("/count", stream_body=True)
def count(req: veil.Request):
    # 本文を溜めずに届いた順に読む
    return str(sum(len(chunk) for chunk in req.stream()))


L4T, L4U = os.environ.get("VEIL_L4_TCP"), os.environ.get("VEIL_L4_UDP")
if L4T:

    @veil.l4_tcp("py_tcp_echo", L4T)
    class Echo(veil.Protocol):
        def data_received(self, data):
            self.transport.write(data)


if L4U:

    @veil.l4_udp("py_udp_echo", L4U)
    def udp_echo(data, peer):
        return b"echo:" + data


if __name__ == "__main__":
    veil.run(listen_cleartext=os.environ.get("VEIL_LISTEN", "127.0.0.1:8000"), overrides={"logging.level": "warn"})
