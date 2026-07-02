#!/usr/bin/env python3
"""HTTP/1.1 プロトコル境界の簡易ファジング（コンテナ内完結）。"""
from __future__ import annotations

import os
import random
import socket
import sys
import time
from pathlib import Path

VEIL_HOST = os.environ.get("VEIL_HOST", "veil-proxy")
VEIL_HTTP_PORT = int(os.environ.get("VEIL_HTTP_PORT", "80"))
VEIL_HTTPS_PORT = int(os.environ.get("VEIL_HTTPS_PORT", "443"))
RESULTS = Path("/results/fuzz_report.txt")
FIXTURES = Path("/fixtures/seeds")

# シードコーパス（最小再現用）
BUILTIN_SEEDS: list[bytes] = [
    b"GET / HTTP/1.1\r\nHost: test\r\n\r\n",
    b"GET / HTTP/1.0\r\n\r\n",
    b"GET /../../../etc/passwd HTTP/1.1\r\nHost: test\r\n\r\n",
    b"GET /%00 HTTP/1.1\r\nHost: test\r\n\r\n",
    b"POST / HTTP/1.1\r\nHost: test\r\nContent-Length: 9999\r\n\r\n",
    b"GET / HTTP/1.1\r\nHost: " + b"A" * 8192 + b"\r\n\r\n",
    b"\x00\x01\x02\xff\xfe",
    b"GET / HTTP/9.9\r\nHost: test\r\n\r\n",
    b"HEAD / HTTP/1.1\r\nHost: test\r\n\r\n",
    b"OPTIONS * HTTP/1.1\r\nHost: test\r\n\r\n",
    b"GET / HTTP/1.1\r\nTransfer-Encoding: chunked\r\nHost: test\r\n\r\n0\r\n\r\n",
    b"",
]


def load_seeds() -> list[bytes]:
    seeds = list(BUILTIN_SEEDS)
    if FIXTURES.is_dir():
        for path in sorted(FIXTURES.iterdir()):
            if path.is_file():
                seeds.append(path.read_bytes())
    return seeds


def mutate(data: bytes, rng: random.Random) -> bytes:
    if not data:
        return bytes(rng.getrandbits(8) for _ in range(rng.randint(1, 64)))
    buf = bytearray(data)
    op = rng.randint(0, 3)
    if op == 0 and buf:
        buf[rng.randrange(len(buf))] = rng.randint(0, 255)
    elif op == 1:
        pos = rng.randrange(0, len(buf) + 1)
        buf[pos:pos] = bytes(rng.getrandbits(8) for _ in range(rng.randint(1, 32)))
    elif op == 2 and buf:
        start = rng.randrange(len(buf))
        end = min(len(buf), start + rng.randint(1, max(1, len(buf) - start)))
        del buf[start:end]
    else:
        buf.extend(bytes(rng.getrandbits(8) for _ in range(rng.randint(1, 128)))
    return bytes(buf)


def send_raw(host: str, port: int, payload: bytes, timeout: float = 2.0) -> tuple[bool, str]:
    try:
        with socket.create_connection((host, port), timeout=timeout) as sock:
            if payload:
                sock.sendall(payload)
            try:
                sock.recv(4096)
            except socket.timeout:
                pass
        return True, "ok"
    except OSError as exc:
        return False, str(exc)


def verify_health() -> bool:
    try:
        with socket.create_connection((VEIL_HOST, VEIL_HTTP_PORT), timeout=3.0) as sock:
            sock.sendall(b"GET / HTTP/1.1\r\nHost: health\r\nConnection: close\r\n\r\n")
            resp = sock.recv(4096)
        return b"HTTP/" in resp
    except OSError:
        return False


def main() -> int:
    rng = random.Random(42)
    seeds = load_seeds()
    iterations = int(os.environ.get("FUZZ_ITERATIONS", "200"))
    sent = 0
    errors = 0
    lines: list[str] = []

    for i in range(iterations):
        seed = seeds[i % len(seeds)]
        payload = seed if i < len(seeds) else mutate(seed, rng)
        ok, msg = send_raw(VEIL_HOST, VEIL_HTTP_PORT, payload)
        sent += 1
        if not ok:
            errors += 1
            if errors <= 20:
                lines.append(f"iter={i} error={msg}")
        # HTTPS 境界にも少数送信
        if i % 10 == 0:
            send_raw(VEIL_HOST, VEIL_HTTPS_PORT, payload)

    time.sleep(0.5)
    healthy = verify_health()
    lines.append(f"sent={sent} transport_errors={errors} healthy_after_fuzz={healthy}")

    RESULTS.parent.mkdir(parents=True, exist_ok=True)
    RESULTS.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print("\n".join(lines))

    if not healthy:
        print("fuzz: veil がファジング後に応答不能", file=sys.stderr)
        return 1
    print("fuzz: ok")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())