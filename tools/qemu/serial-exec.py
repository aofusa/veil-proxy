#!/usr/bin/env python3
"""BSD ゲストのシリアルコンソール（qemu の telnet ソケット）へ root ログインして
任意のコマンドを実行する汎用ヘルパ。

SSH がまだ上がっていない／壊れている状況の**切り分けと復旧**に使う（実際に、
UFS が unclean で `/` が read-only マウントされ sshd が機能しない状態の復旧に必要だった）。

前提:
  - qemu が `-serial telnet:0.0.0.0:<CON_PORT>,server,nowait` で起動している
  - ゲストの root パスワードが分かっている（bsd-vm.sh の `VM_ROOT_PASSWORD`、既定 `veil`）
  - telnet の IAC(0xff) が混ざるため encoding は latin-1

使い方:
  python3 tools/qemu/serial-exec.py --con-port 2311 'fsck -y /dev/gpt/rootfs' 'mount -u -w /'
  echo 'df -h' | python3 tools/qemu/serial-exec.py --con-port 2311 --stdin
"""
import argparse
import socket
import sys
import time

from pexpect import fdpexpect, TIMEOUT  # type: ignore

PROMPT = r"(?:root@[^#\r\n]*# |\r\n# )"


def connect(con_port: int):
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    for _ in range(60):
        try:
            s.connect(("127.0.0.1", con_port))
            return s
        except OSError:
            time.sleep(2)
    print("cannot connect to console TCP %d" % con_port, flush=True)
    sys.exit(1)


def login(child, user: str, password: str, timeout: int) -> None:
    child.sendline("")
    i = child.expect([r"login:", PROMPT, TIMEOUT], timeout=timeout)
    if i == 2:
        print("TIMEOUT waiting for login prompt", flush=True)
        sys.exit(1)
    if i == 0:
        child.sendline(user)
        j = child.expect([r"[Pp]assword:", PROMPT, TIMEOUT], timeout=120)
        if j == 0:
            child.sendline(password)
            child.expect([PROMPT, TIMEOUT], timeout=180)
        elif j == 2:
            print("TIMEOUT after sending the login name", flush=True)
            sys.exit(1)
    print("LOGGED_IN", flush=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--con-port", type=int, required=True)
    ap.add_argument("--user", default="root")
    ap.add_argument("--password", default="veil")
    ap.add_argument("--timeout", type=int, default=600, help="1 コマンドあたりの待ち時間(秒)")
    ap.add_argument("--login-timeout", type=int, default=600)
    ap.add_argument("--stdin", action="store_true", help="標準入力の各行をコマンドとして実行する")
    ap.add_argument("cmds", nargs="*")
    args = ap.parse_args()

    cmds = list(args.cmds)
    if args.stdin:
        cmds += [ln.rstrip("\n") for ln in sys.stdin if ln.strip()]
    if not cmds:
        ap.error("コマンドが指定されていない")

    s = connect(args.con_port)
    child = fdpexpect.fdspawn(s.fileno(), encoding="latin-1", timeout=args.timeout)
    child.logfile_read = sys.stdout
    login(child, args.user, args.password, args.login_timeout)

    for cmd in cmds:
        child.sendline(cmd)
        if child.expect([PROMPT, TIMEOUT], timeout=args.timeout) == 1:
            print("\nTIMEOUT running: %s" % cmd, flush=True)
            sys.exit(1)
    print("\nSERIAL_EXEC_DONE", flush=True)
    s.close()


if __name__ == "__main__":
    main()
