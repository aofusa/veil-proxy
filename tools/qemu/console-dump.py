#!/usr/bin/env python3
"""QEMU のシリアルコンソール（telnet）へ繋いで出力を標準出力へ流す（B-47）。

`bsd-vm.sh <os> <arch> console [秒数]` から呼ばれる。VM の起動ログや
インストーラの画面を非対話で覗くための最小ツール。
telnet の IAC(0xff) が混ざるため latin-1 でデコードする。
"""
import socket
import sys

def main() -> None:
    port = int(sys.argv[1])
    sock = socket.socket()
    sock.settimeout(2)
    try:
        sock.connect(("127.0.0.1", port))
    except OSError as exc:
        sys.exit("console unreachable on 127.0.0.1:%d: %s" % (port, exc))
    while True:
        try:
            data = sock.recv(16384)
        except OSError:
            break
        if not data:
            break
        sys.stdout.write(data.decode("latin-1"))
        sys.stdout.flush()

if __name__ == "__main__":
    main()
