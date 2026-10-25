#!/usr/bin/env python3
"""HTTP/1.1 upgrade checks against examples/api/rust-websocket (F-261). Used by tests/api/library_e2e.sh.

    upgrade_check.py basic <port> [tls]      first line in the same packet, pipelining, 400/500 cases
    upgrade_check.py many <port> <n> [tls]   n upgraded connections open at the same time
    upgrade_check.py hold <port> <n>         opens n upgraded connections, prints "held", waits for "bye" + EOF
    upgrade_check.py refused <port>          a new connection is closed without a response

Prints one "ok <name>" / "FAIL <name>: ..." line per check and exits non-zero on any failure.
"""
import asyncio
import resource
import socket
import ssl
import sys
import time

FAIL = 0


def ok(name):
    print(f"ok {name}", flush=True)


def bad(name, why):
    global FAIL
    FAIL = 1
    print(f"FAIL {name}: {why}", flush=True)


def check(name, cond, why=""):
    if cond:
        ok(name)
    else:
        bad(name, why)


def connect(port, tls):
    s = socket.create_connection(("127.0.0.1", port), timeout=5)
    if tls:
        ctx = ssl.create_default_context()
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
        s = ctx.wrap_socket(s, server_hostname="localhost")
    return s


def read_head(s):
    data = b""
    while b"\r\n\r\n" not in data:
        b = s.recv(65536)
        if not b:
            break
        data += b
    head, _, rest = data.partition(b"\r\n\r\n")
    return head.decode("latin-1"), rest


def read_until(s, want, rest=b""):
    data = rest
    while want not in data:
        b = s.recv(65536)
        if not b:
            break
        data += b
    return data


UPGRADE = b"GET /echo HTTP/1.1\r\nHost: x\r\nConnection: keep-alive, Upgrade\r\nUpgrade: x-echo\r\n\r\n"


def basic(port, tls):
    t = " (tls)" if tls else ""
    # 1. ヘッダと同じパケットで送った最初の行が失われない
    s = connect(port, tls)
    s.sendall(UPGRADE + b"first\n")
    head, rest = read_head(s)
    if head.startswith("HTTP/1.1 101 ") and "upgrade: x-echo" in head.lower():
        ok("101 Switching Protocols" + t)
    else:
        bad("101 Switching Protocols" + t, head)
    low = head.lower()
    if "content-length" in low or "keep-alive" in low or "x-echo-version: 1" not in low:
        bad("101 head has no Content-Length / keep-alive, keeps handler headers" + t, head)
    else:
        ok("101 head has no Content-Length / keep-alive, keeps handler headers" + t)
    got = read_until(s, b"first\n", rest)
    check("first line in the same packet" + t, got.endswith(b"first\n"), got)
    s.sendall(b"second\nthird\n")
    got = read_until(s, b"third\n")
    check("echo after upgrade" + t, got == b"second\nthird\n", got)
    s.close()
    # 2. パイプライン: 先の通常の要求に応答し、後ろのアップグレードの後のバイト列はプロトコルのもの
    s = connect(port, tls)
    s.sendall(
        b"GET /plaintext HTTP/1.1\r\nHost: x\r\n\r\n"
        + UPGRADE
        + b"GET /plaintext HTTP/1.1\r\nHost: x\r\n\r\n"
    )
    data = read_until(s, b"Host: x\r\n\r\n")
    if (
        data.startswith(b"HTTP/1.1 200 ")
        and b"Hello, World!" in data
        and b"HTTP/1.1 101 " in data
        and data.endswith(b"GET /plaintext HTTP/1.1\r\nHost: x\r\n\r\n")
        and data.count(b"Hello, World!") == 1
    ):
        ok("pipelined request after the upgrade is protocol data" + t)
    else:
        bad("pipelined request after the upgrade is protocol data" + t, data[:400])
    s.close()
    # 3. アップグレードでない要求は 400、予約済みの h2c は 500
    cases = [
        ("no Upgrade header -> 400", b"GET /raw-upgrade HTTP/1.1\r\nHost: x\r\n\r\n", "400"),
        (
            "request with a body -> 400",
            b"GET /raw-upgrade HTTP/1.1\r\nHost: x\r\nConnection: upgrade\r\nUpgrade: x-echo\r\n"
            b"Content-Length: 3\r\n\r\nabc",
            "400",
        ),
        (
            "HTTP/1.0 -> 400",
            b"GET /raw-upgrade HTTP/1.0\r\nHost: x\r\nConnection: upgrade\r\nUpgrade: x-echo\r\n\r\n",
            "400",
        ),
        (
            "reserved h2c -> 500",
            b"GET /h2c-upgrade HTTP/1.1\r\nHost: x\r\nConnection: upgrade\r\nUpgrade: h2c\r\n\r\n",
            "500",
        ),
        ("handler declines -> 426", b"GET /echo HTTP/1.1\r\nHost: x\r\n\r\n", "426"),
    ]
    for name, req, status in cases:
        s = connect(port, tls)
        s.sendall(req)
        head, _ = read_head(s)
        check(name + t, head.startswith(f"HTTP/1.1 {status} "), head[:200])
        s.close()


def raise_nofile(n):
    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    want = min(hard, max(soft, n + 256))
    resource.setrlimit(resource.RLIMIT_NOFILE, (want, hard))


async def many(port, n, tls):
    raise_nofile(n)
    ctx = None
    if tls:
        ctx = ssl.create_default_context()
        ctx.check_hostname = False
        ctx.verify_mode = ssl.CERT_NONE
    sem = asyncio.Semaphore(200)

    async def open_one(i):
        async with sem:
            r, w = await asyncio.open_connection(
                "127.0.0.1", port, ssl=ctx, server_hostname="localhost" if tls else None
            )
            w.write(UPGRADE)
            await w.drain()
            await r.readuntil(b"\r\n\r\n")
            return r, w

    conns = await asyncio.wait_for(asyncio.gather(*(open_one(i) for i in range(n))), 120)

    async def echo(i, rw):
        r, w = rw
        line = f"conn {i}\n".encode()
        w.write(line)
        await w.drain()
        return await r.readline() == line

    res = await asyncio.wait_for(asyncio.gather(*(echo(i, c) for i, c in enumerate(conns))), 120)
    name = f"{n} upgraded connections at once" + (" (tls)" if tls else "")
    check(name, all(res), f"{res.count(False)} failed")
    for _, w in conns:
        w.close()


def hold(port, n):
    socks = []
    for _ in range(n):
        # 起動確認の接続が数から外れる前だと上限で切られるので、少し待って開き直す
        for attempt in range(50):
            try:
                s = connect(port, False)
                s.sendall(UPGRADE)
                head, _ = read_head(s)
                if head.startswith("HTTP/1.1 101 "):
                    break
            except OSError:
                pass
            time.sleep(0.2)
        socks.append(s)
    print("held", flush=True)
    for s in socks:
        s.settimeout(30)
    good = 0
    for s in socks:
        try:
            got = read_until(s, b"bye\n")
            if got == b"bye\n" and s.recv(16) == b"":
                good += 1
        except OSError:
            pass
    check("graceful shutdown closes upgraded connections", good == n, f"{good}/{n}")


def refused(port):
    s = socket.create_connection(("127.0.0.1", port), timeout=5)
    try:
        s.sendall(b"GET /plaintext HTTP/1.1\r\nHost: x\r\n\r\n")
        data = s.recv(100)
    except OSError:
        data = b""
    check("connection over max_concurrent_connections is refused", data == b"", data)


def main():
    mode, port = sys.argv[1], int(sys.argv[2])
    if mode == "basic":
        basic(port, len(sys.argv) > 3)
    elif mode == "many":
        asyncio.run(many(port, int(sys.argv[3]), len(sys.argv) > 4))
    elif mode == "hold":
        hold(port, int(sys.argv[3]))
    elif mode == "refused":
        refused(port)
    sys.exit(FAIL)


if __name__ == "__main__":
    main()
