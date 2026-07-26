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
    """ローダーメニュー（beastie）で single user（オプション 2）を選ぶ。

    メニューは再描画が激しく、1 回の "2" は取りこぼされて autoboot がそのまま
    multi-user へ進んでしまうことがある（実測）。そこで
      1. Space で autoboot のカウントダウンを止め、
      2. "2" を複数回送る
    という手順にしている。
    """
    i = child.expect([r"Boot Multi user", r"Autoboot in", TIMEOUT], timeout=300)
    if i == 2:
        print("TIMEOUT waiting for loader menu", flush=True)
        sys.exit(1)
    # autoboot のカウントダウンを止める（メニューが確実に操作可能になる）
    time.sleep(0.5)
    child.send(" ")
    time.sleep(1.5)

    # amd64 では **ローダプロンプトから `console="comconsole"` を設定してから**
    # single-user 起動する必要がある。
    # SeaBIOS 経路では SeaBIOS/boot2/ローダの出力はシリアルへ流れるが、
    # FreeBSD **カーネル**は既定で vidconsole しか使わないため、メニューで "2" を
    # 選んだだけではカーネル以降の出力・入力がシリアルに出てこない（実測）。
    # 入力もシリアル経由なので、ここで comconsole へ切り替えても操作系は生き続ける。
    child.send("3")  # Escape to loader prompt
    child.expect([r"OK ", r"OK"], timeout=120)
    child.sendline('set console="comconsole"')
    time.sleep(1.5)
    # コンソール切り替え直後は最初の 1 文字が落ちる（`boot -s` が `oot -s` になり
    # "unknown command" になる）。空行を 1 回送ってプロンプトを出し直してから本命を送る。
    child.sendline("")
    time.sleep(1.0)
    # `-h` (RB_SERIAL) を付けて**カーネル**にもシリアルコンソールを使わせる。
    # ローダの `console="comconsole"` だけではローダまでの出力しかシリアルに出ず、
    # カーネル以降が vidconsole のままになる（実測）。
    child.sendline("boot -s -h")

    j = child.expect([r"Enter full pathname of shell.*:", r"\r\n# ", TIMEOUT], timeout=300)
    if j == 2:
        print("TIMEOUT waiting for single-user shell", flush=True)
        sys.exit(1)
    if j == 0:
        child.sendline("")
    child.expect([r"\r\n# "], timeout=120)
    print("ROOT_SHELL_SU", flush=True)


def run(child, cmd, t=300):
    child.sendline(cmd)
    child.expect([r"\r\n# "], timeout=t)
    return child.before


def login_and_inject_key(child, password: str, pubkey_path: str) -> None:
    """multi-user のシリアル getty へログインし、root の SSH 公開鍵を設置する。"""
    pub = open(pubkey_path).read().strip()

    i = child.expect([r"login:", r"root@[^#]*# ", TIMEOUT], timeout=1800)
    if i == 2:
        print("TIMEOUT waiting for the login prompt", flush=True)
        sys.exit(1)
    if i == 0:
        child.sendline("root")
        j = child.expect([r"[Pp]assword:", r"root@[^#]*# ", TIMEOUT], timeout=120)
        if j == 0:
            child.sendline(password)
            child.expect([r"root@[^#]*# ", r"\r\n# "], timeout=180)
        elif j == 2:
            print("TIMEOUT after sending the login name", flush=True)
            sys.exit(1)
    print("LOGGED_IN", flush=True)

    def run(cmd, t=180):
        child.sendline(cmd)
        child.expect([r"root@[^#]*# ", r"\r\n# "], timeout=t)
        return child.before

    run("mkdir -p /root/.ssh && chmod 700 /root/.ssh")
    run("printf '%%s\\n' '%s' > /root/.ssh/authorized_keys" % pub)
    run("chmod 600 /root/.ssh/authorized_keys")
    run("sysrc sshd_enable=YES")
    run("grep -q '^PermitRootLogin prohibit-password' /etc/ssh/sshd_config || "
        "echo 'PermitRootLogin prohibit-password' >> /etc/ssh/sshd_config")
    run("grep -q '^PubkeyAuthentication yes' /etc/ssh/sshd_config || "
        "echo 'PubkeyAuthentication yes' >> /etc/ssh/sshd_config")
    run("service sshd restart || service sshd start", t=180)
    run("wc -l /root/.ssh/authorized_keys")
    run("sync")
    print("PROVISIONED_SSH", flush=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--mode", choices=["ssh", "grow", "login"], required=True)
    ap.add_argument("--con-port", type=int, default=2224)
    ap.add_argument("--pubkey", default=os.path.expanduser("~/.ssh/veil_qemu_key.pub"))
    ap.add_argument("--dev", default="/dev/gpt/rootfs")
    ap.add_argument("--password", default="veil",
                    help="login モードで使う root パスワード（cloud-init の chpasswd で設定した値）")
    args = ap.parse_args()

    s = connect(args.con_port)
    child = fdpexpect.fdspawn(s.fileno(), encoding="latin-1", timeout=600)
    child.logfile_read = sys.stdout
    child.sendline("")

    if args.mode == "login":
        # multi-user の getty へ root/パスワードでログインして鍵を注入する。
        #
        # cloud-init（BASIC-CLOUDINIT イメージ）は `chpasswd` は適用するが
        # `write_files` / `runcmd` は実行されない（FreeBSD 版は有効モジュールが
        # 限定的。実測）。そのため鍵注入だけはコンソールから行う。
        login_and_inject_key(child, args.password, args.pubkey)
        s.close()
        return

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
