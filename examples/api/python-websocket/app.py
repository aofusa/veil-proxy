"""veil の Python の常駐のイベントループ・WebSocket・アップグレード・lifespan（F-263）。

- ``/ws``: WebSocket のエコー（テキスト・バイナリ。Autobahn TestSuite の対象）
- ``/chat``: 受け取ったテキストを全員へ配る（FastAPI の文書のチャットと同じ書き方）
- ``/typed``: JSON のメッセージ（pydantic があれば OpenAPI の ``x-veil-websocket`` に型が載り、型に合わなければ 1007）
- ``/noaccept``: accept せずに戻る（403）
- ``/endpoint``: ``WebSocketEndpoint`` のクラス（コールバックはハンドラ用プール）
- ``/echo``: 独自プロトコル ``x-echo`` へのアップグレード（``veil.AsyncProtocol``）
- ``/sleep``: ``await asyncio.sleep(1)`` の要求（常駐のループなので同時に何本でも待てる）
- ``/state``: lifespan が作った値（``app.state``）、``/threadpool``: ``run_in_threadpool``

    VEIL_LISTEN=127.0.0.1:8000 python app.py
"""

import asyncio
import contextlib
import os
import time

import veil

try:
    import pydantic
except ImportError:  # pydantic が無くても他は動く
    pydantic = None


@contextlib.asynccontextmanager
async def lifespan(app):
    # リスナーを開く前にループで走る（ここで例外なら起動しない）
    yield {"greeting": "hello from lifespan", "started": time.time()}
    print("lifespan: shutdown", flush=True)


app = veil.App(title="veil websocket example", lifespan=lifespan)


@app.websocket("/ws", subprotocols=["echo.v1"], tags=["websocket"])
async def ws(websocket: veil.WebSocket):
    """Echo (text and binary)."""
    sub = "echo.v1" if "echo.v1" in websocket.subprotocols else None
    await websocket.accept(subprotocol=sub)
    while True:
        m = await websocket.receive()
        if m["type"] == "websocket.disconnect":
            return
        if m.get("text") is not None:
            await websocket.send_text(m["text"])
        else:
            await websocket.send_bytes(m["bytes"])


class ConnectionManager:
    def __init__(self):
        self.active = []

    async def connect(self, websocket):
        await websocket.accept()
        self.active.append(websocket)

    def disconnect(self, websocket):
        self.active.remove(websocket)

    async def broadcast(self, message):
        for c in list(self.active):
            await c.send_text(message)


manager = ConnectionManager()


@app.websocket("/chat")
async def chat(websocket: veil.WebSocket):
    await manager.connect(websocket)
    try:
        while True:
            data = await websocket.receive_text()
            await manager.broadcast(data)
    except veil.WebSocketDisconnect:
        manager.disconnect(websocket)


if pydantic is not None:

    class ChatIn(pydantic.BaseModel):
        room: str
        text: str

    class ChatOut(pydantic.BaseModel):
        room: str
        text: str
        length: int

    @app.websocket("/typed", receive=ChatIn, send=ChatOut, validate=True, tags=["websocket"])
    async def typed(websocket: veil.WebSocket):
        """Typed JSON messages."""
        await websocket.accept()
        async for m in websocket.iter_json():
            await websocket.send_json({"room": m["room"], "text": m["text"], "length": len(m["text"])})


@app.websocket("/noaccept")
async def noaccept(websocket: veil.WebSocket):
    return


@app.websocket("/hidden", include_in_schema=False)
async def hidden(websocket: veil.WebSocket):
    await websocket.accept()
    await websocket.close()


@app.websocket("/endpoint")
class Echo(veil.WebSocketEndpoint):
    encoding = "text"

    def on_connect(self, websocket):
        websocket.send_text("welcome")

    def on_receive(self, websocket, data):
        websocket.send_text("endpoint:" + data)


class LineEcho(veil.AsyncProtocol):
    def connection_made(self, transport):
        self.transport = transport

    async def data_received(self, data):
        await asyncio.sleep(0)
        self.transport.write(data)


@app.get("/echo")
def echo(req: veil.Request):
    return veil.Response.switching_protocols("x-echo", LineEcho)


@app.get("/sleep")
async def sleep():
    await asyncio.sleep(1)
    return "slept"


@app.get("/pool-sleep", async_mode="pool")
async def pool_sleep():
    await asyncio.sleep(0.01)
    return "pool"


@app.get("/state")
async def state():
    return {"greeting": app.state.greeting}


@app.get("/threadpool")
async def threadpool():
    return await veil.run_in_threadpool(lambda: "from pool")


if __name__ == "__main__":
    app.run(listen_cleartext=os.environ.get("VEIL_LISTEN", "127.0.0.1:8000"))
