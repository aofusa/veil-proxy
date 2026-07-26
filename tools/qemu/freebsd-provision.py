#!/usr/bin/env python3
"""FreeBSD arm64 VM のシリアルコンソール provision（single-user 経由）。

FreeBSD arm64 VM-IMAGE はシリアルに getty が無く root SSH も既定無効のため、
qemu の telnet シリアルコンソール（TCP）へ pexpect で接続し、**loader メニューで
single-user（"2"）** を選んで getty 不要の root シェルを得てから設定する。

モード:
  ssh   : / を rw 再マウントして SSH 公開鍵を注入 + sshd 有効化 + PermitRootLogin yes、
          exit で multi-user 継続（VM 起動直後に使う）。
  grow  : / を ro 再マウント → fsck → growfs（`qemu-img resize` 後にディスクを拡張。
          single-user では / が clean にできるので online 不可の growfs が通る）、reboot。

前提: qemu を `-serial telnet:0.0.0.0:<CON_PORT>,server,nowait` で起動していること。
      telnet の IAC(0xff) があるため encoding は latin-1 を使う。

使い方:
  python3 bsd-arm64-provision.py --mode ssh  --con-port 2224 --pubkey ~/.ssh/id_qemu.pub
  python3 bsd-arm64-provision.py --mode grow --con-port 2224
"""
import argparse
import os
import socket
import sys
import time

from pexpect import fdpexpect, TIMEOUT  # type: ignore


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


def enter_single_user(child):
    # ローダーメニュー（beastie）で single user（オプション 2）を選ぶ。
    i = child.expect([r"Boot Multi user", r"Autoboot in", TIMEOUT], timeout=300)
    if i == 2:
        print("TIMEOUT waiting for loader menu", flush=True)
        sys.exit(1)
    time.sleep(1)
    child.send("2")
    j = child.expect([r"Enter full pathname of shell.*:", r"\r\n# ", TIMEOUT], timeout=300)
    if j == 0:
        child.sendline("")
    child.expect([r"\r\n# "], timeout=120)
    print("ROOT_SHELL_SU", flush=True)


def run(child, cmd, t=300):
    child.sendline(cmd)
    child.expect([r"\r\n# "], timeout=t)
    return child.before


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--mode", choices=["ssh", "grow"], required=True)
    ap.add_argument("--con-port", type=int, default=2224)
    ap.add_argument("--pubkey", default=os.path.expanduser("~/.ssh/veil_qemu_key.pub"))
    ap.add_argument("--dev", default="/dev/gpt/rootfs")
    args = ap.parse_args()

    s = connect(args.con_port)
    child = fdpexpect.fdspawn(s.fileno(), encoding="latin-1", timeout=600)
    child.logfile_read = sys.stdout
    child.sendline("")
    enter_single_user(child)

    if args.mode == "ssh":
        pub = open(args.pubkey).read().strip()
        run(child, "mount -u -o rw / ; mount -a")
        run(child, "mkdir -p /root/.ssh && chmod 700 /root/.ssh")
        run(child, "printf '%%s\\n' '%s' > /root/.ssh/authorized_keys && chmod 600 /root/.ssh/authorized_keys" % pub)
        run(child, "sysrc sshd_enable=YES")
        run(child, "grep -q '^PermitRootLogin yes' /etc/ssh/sshd_config || echo 'PermitRootLogin yes' >> /etc/ssh/sshd_config")
        run(child, "grep -q '^PubkeyAuthentication yes' /etc/ssh/sshd_config || echo 'PubkeyAuthentication yes' >> /etc/ssh/sshd_config")
        run(child, "sync")
        print("PROVISIONED_SSH", flush=True)
        child.sendline("exit")  # multi-user へ継続
        time.sleep(3)
    elif args.mode == "grow":
        # single-user では / を ro にして fsck→growfs（clean 必須）。
        run(child, "mount -u -o ro /", t=60)
        run(child, "fsck -y %s" % args.dev, t=600)
        out = run(child, "growfs -y %s" % args.dev, t=900)
        print("GROWFS_OUT:", out[-200:], flush=True)
        run(child, "mount -u -o rw / ; df -h / ; sync")
        print("GROWFS_DONE", flush=True)
        child.sendline("reboot")
        time.sleep(3)
    s.close()


if __name__ == "__main__":
    main()
