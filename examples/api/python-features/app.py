"""veil の Python の F-210 の機能: ストリーミング応答（ジェネレータ・SSE・非同期ジェネレータ）・
要求本文のストリーミング受信・L4（TCP・UDP のエコー）・フォームとファイルのアップロード（F-266）・
応答の圧縮（``veil.run(compression=True)``・``compress=False``。F-270）。

    VEIL_LISTEN=127.0.0.1:8000 VEIL_L4_TCP=127.0.0.1:7000 VEIL_L4_UDP=127.0.0.1:7001 python app.py
"""

import asyncio
import hashlib
import io
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


@veil.post("/upload")
async def upload(
    title: str = veil.Form(),
    count: int = veil.Form(1),
    doc: veil.UploadFile = veil.File(),
    note: veil.UploadFile = None,
):
    # 小さいファイルはメモリ、[api] multipart_spool_threshold を超えるものは名前の無い一時ファイル
    h = hashlib.sha256()
    while True:
        chunk = await doc.read(1 << 20)
        if not chunk:
            break
        h.update(chunk)
    await doc.close()
    return {
        "title": title,
        "count": count,
        "filename": doc.filename,
        "size": doc.size,
        "sha256": h.hexdigest(),
        "where": "memory" if isinstance(doc.file, io.BytesIO) else "disk",
        "note": note.read_bytes().decode() if note else None,
    }


@veil.post("/upload-many")
def upload_many(docs: list[veil.UploadFile]):
    return [{"filename": d.filename, "size": d.size} for d in docs]


@veil.post("/login")
def login(user: str = veil.Form(), remember: bool = veil.Form(False)):
    return {"user": user, "remember": remember}


def _items():
    return {"items": [{"id": i, "name": "item-%d" % i} for i in range(100)]}


@veil.get("/data")
def data():
    # veil.run(compression=True) なので Accept-Encoding に応じて br・gzip で返る（256 バイト未満は圧縮しない）
    return _items()


@veil.get("/data-raw", compress=False)
def data_raw():
    # このルートは圧縮しない
    return _items()


@veil.get("/data-nc")
def data_nc():
    # 応答ごとに止める（veil.Response・JSONResponse の compress=False）
    return veil.JSONResponse(_items(), compress=False)


@veil.get("/lines")
def lines():
    # ストリーミング応答も断片ごとに flush しながら圧縮する
    for i in range(1, 51):
        yield "line %04d %s\n" % (i, "x" * 200)


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
    veil.run(
        listen_cleartext=os.environ.get("VEIL_LISTEN", "127.0.0.1:8000"),
        overrides={"logging.level": "warn", "api.multipart_max_file_size": 209715200},
        compression=True,
    )
