#!/usr/bin/env python3
# F-262: API ライブラリの WebSocket（examples/api/rust-websocket の /ws・/chat・/typed、C の /ws）の確認。
# 標準ライブラリだけで書いた最小の WebSocket クライアント（RFC 6455）。HttpArena の validate-ws の項目を含む。
#
#   ws_check.py basic <port> [tls] [path]   ハンドシェイク・エコー・断片化・Ping・まとめ送り・Close・不正な入力
#   ws_check.py rust <port> [tls]           basic に加えて /chat（ブロードキャスト）・/typed（型の検査）・サブプロトコル
#   ws_check.py many <port> <n> [tls]       n 本を同時に開いてエコー（16,384 本は ulimit -n を上げて）
#   ws_check.py churn <port> <n>            10 メッセージごとに切断して開き直すのを n 回（ハンドシェイクの費用）
#   ws_check.py hold <port> <secs>          開いたまま待ち、graceful shutdown の Close 1001 を受けたら "close 1001"
import asyncio
import base64
import hashlib
import os
import resource
import ssl
import struct
import sys
import time

GUID = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
results = []


def ok(name):
    results.append(True)
    print(f"ok   {name}", flush=True)


def bad(name, why=""):
    results.append(False)
    print(f"FAIL {name} {why}", flush=True)


def check(name, cond, why=""):
    ok(name) if cond else bad(name, why)


class WS:
    def __init__(self, r, w, head):
        self.r, self.w, self.head = r, w, head

    @staticmethod
    async def connect(port, path="/ws", tls=False, extra=b"", key=None):
        # 文脈は使い回す（作るたびに CA の束を読むので、大量の接続ではそれだけで握手が時間切れになる）
        ctx = tls_ctx() if tls else None
        r, w = await asyncio.open_connection("127.0.0.1", port, ssl=ctx, server_hostname="localhost" if tls else None, limit=1 << 26)
        key = key or base64.b64encode(os.urandom(16))
        w.write(b"GET " + path.encode() + b" HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\n"
                b"Connection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: " + key + b"\r\n" + extra + b"\r\n")
        await w.drain()
        head = await r.readuntil(b"\r\n\r\n")
        ws = WS(r, w, head)
        ws.key = key
        return ws

    def status(self):
        return int(self.head.split(b" ", 2)[1])

    def header(self, name):
        for line in self.head.split(b"\r\n")[1:]:
            n, _, v = line.partition(b":")
            if n.strip().lower() == name.lower():
                return v.strip()
        return None

    def frame(self, opcode, payload=b"", fin=True, mask=True, rsv=0):
        b0 = (0x80 if fin else 0) | rsv | opcode
        n = len(payload)
        m = 0x80 if mask else 0
        if n < 126:
            h = struct.pack("!BB", b0, m | n)
        elif n < 65536:
            h = struct.pack("!BBH", b0, m | 126, n)
        else:
            h = struct.pack("!BBQ", b0, m | 127, n)
        if not mask:
            return h + payload
        k = os.urandom(4)
        return h + k + bytes(c ^ k[i & 3] for i, c in enumerate(payload)) if n < 4096 else h + k + xor(payload, k)

    async def send(self, opcode, payload=b"", **kw):
        self.w.write(self.frame(opcode, payload, **kw))
        await self.w.drain()

    async def recv(self, timeout=5):
        h = await asyncio.wait_for(self.r.readexactly(2), timeout)
        op, n = h[0] & 0x0F, h[1] & 0x7F
        if n == 126:
            n = struct.unpack("!H", await self.r.readexactly(2))[0]
        elif n == 127:
            n = struct.unpack("!Q", await self.r.readexactly(8))[0]
        if h[1] & 0x80:
            raise ValueError("server frame is masked")
        data = await asyncio.wait_for(self.r.readexactly(n), timeout)
        return op, data, bool(h[0] & 0x80)

    async def recv_close(self, timeout=5):
        while True:
            op, data, _ = await self.recv(timeout)
            if op == 8:
                return struct.unpack("!H", data[:2])[0] if len(data) >= 2 else 1005

    async def eof(self, timeout=5):
        try:
            return await asyncio.wait_for(self.r.read(1), timeout) == b""
        except (ConnectionError, asyncio.IncompleteReadError):
            return True

    def close(self):
        self.w.close()


def xor(p, k):
    m = (k * (len(p) // 4 + 1))[: len(p)]
    return (int.from_bytes(p, "big") ^ int.from_bytes(m, "big")).to_bytes(len(p), "big")


async def basic(port, tls, path):
    sfx = " (tls)" if tls else ""
    # 接続ごとに違う Sec-WebSocket-Accept（RFC 6455 §1.3 の計算）
    a = await WS.connect(port, path, tls)
    b = await WS.connect(port, path, tls)
    exp = base64.b64encode(hashlib.sha1(a.key + GUID).digest())
    check("handshake 101 + Sec-WebSocket-Accept" + sfx, a.status() == 101 and a.header(b"sec-websocket-accept") == exp,
          a.head[:200])
    check("distinct accept per connection" + sfx, a.header(b"sec-websocket-accept") != b.header(b"sec-websocket-accept"))
    b.close()
    await a.send(1, b"hello")
    check("text echo" + sfx, await a.recv() == (1, b"hello", True))
    blob = bytes(range(256))
    await a.send(2, blob)
    check("binary echo (256 bytes)" + sfx, await a.recv() == (2, blob, True))
    for i in range(5):
        await a.send(1, f"m{i}".encode())
    got = [(await a.recv())[1] for _ in range(5)]
    check("5 messages echoed in order" + sfx, got == [f"m{i}".encode() for i in range(5)], got)
    # 16 件まとめ送り（1 回の書き込み）
    a.w.write(b"".join(a.frame(1, f"p{i}".encode()) for i in range(16)))
    await a.w.drain()
    got = [(await a.recv())[1] for _ in range(16)]
    check("16 pipelined messages" + sfx, got == [f"p{i}".encode() for i in range(16)])
    # 断片化 + 途中の Ping（UTF-8 の文字を断片の境目で割る）
    t = "héllo wörld".encode()
    await a.send(1, t[:2], fin=False)
    await a.send(9, b"pp")
    await a.send(0, t[2:7], fin=False)
    await a.send(0, t[7:])
    check("pong during fragmented message" + sfx, await a.recv() == (10, b"pp", True))
    check("fragmented text reassembled" + sfx, await a.recv() == (1, t, True))
    big = os.urandom(1 << 20)
    await a.send(2, big)
    check("1 MiB binary echo" + sfx, await a.recv(15) == (2, big, True))
    # Close 1000 に 1000 を返す
    await a.send(8, struct.pack("!H", 1000))
    check("close 1000 answered with 1000" + sfx, await a.recv_close() == 1000)
    check("connection closed after close" + sfx, await a.eof())
    a.close()
    # プロトコル違反
    for name, frames, code in [
        ("unmasked frame -> 1002", [dict(opcode=1, payload=b"x", mask=False)], 1002),
        ("reserved bits -> 1002", [dict(opcode=1, payload=b"x", rsv=0x20)], 1002),
        ("unknown opcode -> 1002", [dict(opcode=3, payload=b"x")], 1002),
        ("invalid UTF-8 -> 1007", [dict(opcode=1, payload=b"\xff\xfe")], 1007),
        ("fragmented control -> 1002", [dict(opcode=9, payload=b"x", fin=False)], 1002),
        ("continuation without start -> 1002", [dict(opcode=0, payload=b"x")], 1002),
        ("close with invalid code -> 1002", [dict(opcode=8, payload=struct.pack("!H", 999))], 1002),
    ]:
        c = await WS.connect(port, path, tls)
        for f in frames:
            await c.send(f.pop("opcode"), f.pop("payload"), **f)
        try:
            got = await c.recv_close()
        except Exception as e:  # noqa: BLE001
            got = repr(e)
        check(name + sfx, got == code, got)
        c.close()
    # アップグレードのヘッダの無い GET は 4xx（400 か 426）
    r, w = await asyncio.open_connection("127.0.0.1", port, ssl=tls_ctx() if tls else None)
    w.write(b"GET " + path.encode() + b" HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
    await w.drain()
    line = (await r.readline()).split(b" ")
    check("plain GET is 4xx" + sfx, int(line[1]) in (400, 426), line)
    w.close()
    c = await WS.connect(port, path, tls, key=b"c2hvcnQ=")
    check("short key is 400" + sfx, c.status() == 400)
    c.close()
    # 試験の後もサーバが応答する
    c = await WS.connect(port, path, tls)
    await c.send(1, b"still")
    check("server still answers" + sfx, await c.recv() == (1, b"still", True))
    c.close()


_CTX = []


def tls_ctx():
    if not _CTX:
        ctx = ssl.create_default_context()
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
        _CTX.append(ctx)
    return _CTX[0]


async def rust(port, tls):
    await basic(port, tls, "/ws")
    sfx = " (tls)" if tls else ""
    a = await WS.connect(port, "/ws", tls, extra=b"Sec-WebSocket-Protocol: x.v9, echo.v1\r\n")
    check("subprotocol selected" + sfx, a.header(b"sec-websocket-protocol") == b"echo.v1")
    a.close()
    # ブロードキャスト（split した送信側を別の接続から使う）
    m = [await WS.connect(port, "/chat", tls) for _ in range(3)]
    await asyncio.sleep(0.2)
    await m[0].send(1, b"hi all")
    got = [await c.recv() for c in m]
    check("chat broadcast to 3 members" + sfx, all(g == (1, b"hi all", True) for g in got), got)
    for c in m:
        c.close()
    t = await WS.connect(port, "/typed", tls)
    await t.send(1, b'{"room":"r","text":"h\xc3\xa9"}')
    op, data, _ = await t.recv()
    check("typed message" + sfx, data == b'{"room":"r","text":"h\xc3\xa9","length":2}', data)
    await t.send(1, b'{"room":1}')
    check("typed message violation -> 1007" + sfx, await t.recv_close() == 1007)
    t.close()


async def many(port, n, tls):
    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    resource.setrlimit(resource.RLIMIT_NOFILE, (min(hard, max(soft, n + 256)), hard))
    sem = asyncio.Semaphore(200)

    async def one(i):
        async with sem:
            c = await WS.connect(port, "/ws", tls)
        return c

    t0 = time.time()
    conns = await asyncio.gather(*(one(i) for i in range(n)), return_exceptions=True)
    live = [c for c in conns if isinstance(c, WS) and c.status() == 101]
    if len(live) != n:
        errs = [c for c in conns if not isinstance(c, WS)][:3]
        bad(f"{n} connections open", f"{len(live)} opened {errs}")
    for i, c in enumerate(live):
        c.w.write(c.frame(1, b"x%d" % i))

    async def back(i, c):
        return await c.recv(30) == (1, b"x%d" % i, True)

    rs = await asyncio.gather(*(back(i, c) for i, c in enumerate(live)), return_exceptions=True)
    good = sum(1 for r in rs if r is True)
    check(f"{n} concurrent connections echo ({time.time() - t0:.1f}s)", good == n, f"{good}/{n}")
    for c in live:
        c.close()


async def churn(port, n):
    for i in range(n):
        c = await WS.connect(port, "/ws")
        for j in range(10):
            await c.send(1, b"c%d" % j)
            if await c.recv() != (1, b"c%d" % j, True):
                return bad("churn", f"round {i}")
        await c.send(8, struct.pack("!H", 1000))
        if await c.recv_close() != 1000:
            return bad("churn close", f"round {i}")
        c.close()
    ok(f"{n} connections x 10 messages with reconnect")


async def hold(port, secs):
    c = await WS.connect(port, "/ws")
    print("held", flush=True)
    try:
        code = await c.recv_close(secs)
        print(f"close {code}", flush=True)
    except Exception as e:  # noqa: BLE001
        print(f"no close {e!r}", flush=True)


def main():
    mode, port = sys.argv[1], int(sys.argv[2])
    args = sys.argv[3:]
    tls = "tls" in args
    if mode == "basic":
        path = next((a for a in args if a.startswith("/")), "/ws")
        asyncio.run(basic(port, tls, path))
    elif mode == "rust":
        asyncio.run(rust(port, tls))
    elif mode == "many":
        asyncio.run(many(port, int(args[0]), tls))
    elif mode == "churn":
        asyncio.run(churn(port, int(args[0])))
    elif mode == "hold":
        asyncio.run(hold(port, float(args[0])))
        return
    sys.exit(0 if results and all(results) else 1)


if __name__ == "__main__":
    main()
