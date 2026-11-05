#!/usr/bin/env python3
"""Independent checks of the C helpers kit's formats (F-291), using only the Python standard library.

  kit_interop.py signed <secret> <name> <value>        print a signed cookie value made like the Rust kit
  kit_interop.py verify-signed <secret> <name> <cookie> print the value when the signature is valid, else "invalid"
  kit_interop.py jwt <secret> <sub> <scope> <iss>      print an HS256 token (exp in 5 minutes)
  kit_interop.py verify-jwt <secret> <token>           print the claims when the HS256 signature is valid
  kit_interop.py ws <port> <path> <protocols> [origin] handshake and print "<status> <protocol> <first text message>"
"""

import base64
import hashlib
import hmac
import json
import os
import socket
import sys
import time


def b64(data: bytes) -> str:
    return base64.urlsafe_b64encode(data).rstrip(b"=").decode()


def unb64(s: str) -> bytes:
    return base64.urlsafe_b64decode(s + "=" * (-len(s) % 4))


def sign_key(secret: str) -> bytes:
    # veil_kit::cookie::Key::derive_from: HMAC-SHA256(secret, "veil-kit cookie signing")
    return hmac.new(secret.encode(), b"veil-kit cookie signing", hashlib.sha256).digest()


def signed(secret: str, name: str, value: str) -> str:
    mac = hmac.new(sign_key(secret), f"{name}={value}".encode(), hashlib.sha256).digest()
    return b64(mac) + "." + value


def verify_signed(secret: str, name: str, cookie: str) -> str:
    mac, _, value = cookie.partition(".")
    want = hmac.new(sign_key(secret), f"{name}={value}".encode(), hashlib.sha256).digest()
    return value if hmac.compare_digest(unb64(mac), want) else "invalid"


def jwt(secret: str, sub: str, scope: str, iss: str) -> str:
    head = b64(json.dumps({"alg": "HS256", "typ": "JWT"}, separators=(",", ":")).encode())
    claims = {"sub": sub, "scope": scope, "iss": iss, "exp": int(time.time()) + 300}
    body = b64(json.dumps(claims, separators=(",", ":")).encode())
    sig = hmac.new(secret.encode(), f"{head}.{body}".encode(), hashlib.sha256).digest()
    return f"{head}.{body}.{b64(sig)}"


def verify_jwt(secret: str, token: str) -> str:
    head, body, sig = token.strip().split(".")
    want = hmac.new(secret.encode(), f"{head}.{body}".encode(), hashlib.sha256).digest()
    if not hmac.compare_digest(unb64(sig), want):
        return "invalid signature"
    if json.loads(unb64(head)).get("alg") != "HS256":
        return "wrong alg"
    return json.dumps(json.loads(unb64(body)), sort_keys=True, separators=(",", ":"))


def ws(port: int, path: str, protocols: str, origin: str = "") -> str:
    s = socket.create_connection(("127.0.0.1", port), timeout=5)
    key = base64.b64encode(os.urandom(16)).decode()
    req = (
        f"GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
        f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n"
    )
    if protocols:
        req += f"Sec-WebSocket-Protocol: {protocols}\r\n"
    if origin:
        req += f"Origin: {origin}\r\n"
    s.sendall((req + "\r\n").encode())
    data = b""
    while b"\r\n\r\n" not in data:
        b = s.recv(4096)
        if not b:
            break
        data += b
    head, _, rest = data.partition(b"\r\n\r\n")
    lines = head.decode(errors="replace").split("\r\n")
    status = lines[0].split(" ")[1] if lines and " " in lines[0] else "?"
    proto = "-"
    for line in lines[1:]:
        k, _, v = line.partition(":")
        if k.strip().lower() == "sec-websocket-protocol":
            proto = v.strip()
    if status != "101":
        return f"{status} {proto} -"
    while len(rest) < 2:
        rest += s.recv(4096)
    n = rest[1] & 0x7F
    while len(rest) < 2 + n:
        rest += s.recv(4096)
    return f"{status} {proto} {rest[2:2 + n].decode()}"


def main() -> None:
    a = sys.argv[1:]
    if a[0] == "signed":
        print(signed(a[1], a[2], a[3]))
    elif a[0] == "verify-signed":
        print(verify_signed(a[1], a[2], a[3]))
    elif a[0] == "jwt":
        print(jwt(a[1], a[2], a[3], a[4]))
    elif a[0] == "verify-jwt":
        print(verify_jwt(a[1], a[2]))
    elif a[0] == "ws":
        print(ws(int(a[1]), a[2], a[3], a[4] if len(a) > 4 else ""))
    else:
        sys.exit("unknown command")


if __name__ == "__main__":
    main()
