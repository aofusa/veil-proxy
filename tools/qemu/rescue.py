#!/usr/bin/env python3
"""シングルユーザーの fsck 待ちで止まった BSD VM をシリアルから戻す（F-176 / tools/qemu）。

異常終了（ホストの OOM・VM の強制停止）の後、FreeBSD / OpenBSD / NetBSD は起動時の fsck が
自動で直せない不整合を見つけるとシングルユーザーのシェル選択で止まる:

    Enter full pathname of shell or RETURN for /bin/sh:   (FreeBSD)
    Enter pathname of shell or RETURN for sh:              (OpenBSD)
    Enter pathname of shell or RETURN for /bin/sh:         (NetBSD)

このスクリプトはシリアル（QEMU の telnet サーバ）へ繋ぎ、プロンプトが出ていれば RETURN →
`fsck -y` → `reboot` を打つ。`login:` が出ている（正常起動している）なら何もしない。
シリアルに何も出ない場合（VGA コンソールの OS）は `bsd-vm.sh <os> <arch> screen` で画面を
確認し、`sendkeys` で操作すること。
"""
import argparse
import socket
import sys
import time

from pexpect import fdpexpect, TIMEOUT  # type: ignore

SHELL_PROMPT = r"pathname of shell or RETURN for"
ROOT_PROMPT = r"(?m)^[^\r\n]*# $"


def connect(port: int, timeout: int = 60) -> socket.socket:
    deadline = time.time() + timeout
    while time.time() < deadline:
        s = socket.socket()
        try:
            s.connect(("127.0.0.1", port))
            return s
        except OSError:
            s.close()
            time.sleep(2)
    sys.exit("cannot connect to console TCP %d" % port)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--con-port", type=int, required=True)
    ap.add_argument("--timeout", type=int, default=120,
                    help="プロンプトを待つ秒数（既定 120）")
    args = ap.parse_args()

    s = connect(args.con_port)
    child = fdpexpect.fdspawn(s.fileno(), encoding="latin-1", timeout=args.timeout)
    child.logfile_read = sys.stdout
    child.sendline("")
    i = child.expect([SHELL_PROMPT, ROOT_PROMPT, r"login:", TIMEOUT], timeout=args.timeout)
    if i == 2:
        print("\nRESCUE: VM is at the login prompt (nothing to do)", flush=True)
        return
    if i == 3:
        sys.exit("RESCUE: no single-user prompt on the serial console "
                 "(check the VGA screen with `bsd-vm.sh <os> <arch> screen`)")
    if i == 0:
        child.sendline("")
        child.expect([ROOT_PROMPT, TIMEOUT], timeout=60)
    print("\nRESCUE: running fsck -y", flush=True)
    child.sendline("fsck -y; echo FSCK_DONE")
    if child.expect([r"FSCK_DONE", TIMEOUT], timeout=3600) != 0:
        sys.exit("RESCUE: fsck did not finish")
    child.expect([ROOT_PROMPT, TIMEOUT], timeout=30)
    child.sendline("sync; reboot")
    time.sleep(5)
    print("\nRESCUE: rebooting", flush=True)
    s.close()


if __name__ == "__main__":
    main()
