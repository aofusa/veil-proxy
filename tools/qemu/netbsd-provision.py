#!/usr/bin/env python3
"""NetBSD x86_64 live image のシリアルコンソール provision（F-140 / tools/qemu）。

FreeBSD の BASIC-CLOUDINIT イメージと違い、NetBSD の live image には cloud-init
相当が無い。そのため FreeBSD の `freebsd-provision.py --mode login` と同じ発想で、
**シリアルコンソールへ root ログインして SSH 公開鍵を直接注入する**。

## 前提（未検証・要確認の点を含む）

- qemu は `-serial telnet:0.0.0.0:<CON_PORT>,server,nowait` で起動していること。
  telnet の IAC(0xff) が混ざるため pexpect の encoding は latin-1。
- NetBSD の live image は既定で **VGA コンソール**へブートメニュー/カーネル出力する
  可能性がある（FreeBSD/OpenBSD の x86_64 と同様の事情）。x86 の NetBSD ブート
  ローダ（boot.cfg）には通常シリアル出力用のメニューエントリ
  （例: `NetBSD (com0)` のような項目）があるとされるが、リリースやビルドにより
  文言が異なりうるため、本スクリプトはブートメニューの `>>` プロンプトを見つけたら
  `consdev com0`（または `consdev 1 com0`）を送ってシリアルへ切り替えを試み、
  見つからなければ**そのまま**ログインプロンプト待ちに進む（既にシリアル出力が
  有効なビルドの場合はこちらに当たる）。
- root のパスワードは live image の既定では**空**（Enter のみ）であることが多いが、
  `--password` で明示指定した値も試す。
- ネットワークは live image が起動時に `dhcpcd` 等で自動設定する想定
  （手動設定が必要な場合は本スクリプトの拡張が要る）。

**このスクリプトは実機コンソールの文言で検証していない（実 VM 起動はコーディ
ネーターが行う）。** タイムアウトや expect パターンが外れた場合は、
`tools/qemu/console-dump.py` でシリアル出力を確認し、実際の文言に合わせて
本スクリプトの正規表現を調整すること。
"""
import argparse
import os
import socket
import sys
import time

from pexpect import fdpexpect, TIMEOUT  # type: ignore


def connect(con_port: int, timeout: int = 180):
    deadline = time.time() + timeout
    while time.time() < deadline:
        s = socket.socket()
        try:
            s.connect(("127.0.0.1", con_port))
            return s
        except OSError:
            s.close()
            time.sleep(2)
    sys.exit("cannot connect to console TCP %d" % con_port)


def try_switch_to_serial(child) -> None:
    """NetBSD x86 のブートメニューでシリアルコンソールへの切り替えを試みる。

    メニューが見えない（＝既にシリアルへ出ている、または起動が速すぎて通過した）
    場合は何もせず戻る。失敗しても致命的ではない設計にしてある。
    """
    i = child.expect([r">>\s*$", r"login:", r"Last message repeated", TIMEOUT], timeout=90)
    if i == 0:
        print("\n(boot prompt '>>' detected; trying 'consdev com0')", flush=True)
        child.sendline("consdev com0")
        time.sleep(1.0)
        child.sendline("boot")
    elif i == 1:
        # 既にログインプロンプトが見えている＝シリアルは既定で有効だった。
        # login() 側で再度 expect するので、ここでは何もしない。
        pass
    else:
        print("\n(boot menu not detected within 90s; assuming serial console is already active)",
              flush=True)


def login_and_inject_key(child, password: str, pubkey_path: str) -> None:
    pub = open(pubkey_path).read().strip()

    i = child.expect([r"login:", r"root@[^#\r\n]*# ", r"[#$] $", TIMEOUT], timeout=1800)
    if i == 3:
        sys.exit("TIMEOUT waiting for the login prompt")
    if i == 0:
        child.sendline("root")
        j = child.expect([r"[Pp]assword:", r"root@[^#\r\n]*# ", r"[#$] $", TIMEOUT], timeout=120)
        if j == 0:
            # live image は root パスワード空の可能性が高いが、明示指定分も試す。
            child.sendline(password)
            k = child.expect([r"[Pp]assword:", r"root@[^#\r\n]*# ", r"[#$] $", TIMEOUT], timeout=60)
            if k == 0:
                # パスワード不一致 → 空文字で再試行
                child.sendline("")
                child.expect([r"root@[^#\r\n]*# ", r"[#$] $"], timeout=60)
        elif j == 3:
            sys.exit("TIMEOUT after sending the login name")
    print("LOGGED_IN", flush=True)

    prompt = r"root@[^#\r\n]*# |[#$] $"

    def run(cmd, t=180):
        child.sendline(cmd)
        idx = child.expect([prompt, TIMEOUT], timeout=t)
        if idx == 1:
            print("\nTIMEOUT running: %s" % cmd, flush=True)
            sys.exit(1)
        return child.before

    run("mkdir -p /root/.ssh && chmod 700 /root/.ssh")
    run("printf '%%s\\n' '%s' > /root/.ssh/authorized_keys" % pub)
    run("chmod 600 /root/.ssh/authorized_keys")
    # NetBSD の /etc/rc.conf は FreeBSD の sysrc に相当する専用コマンドが無いため、
    # grep+echo で追記する（既にあれば重複させない）。
    run("grep -q '^sshd=' /etc/rc.conf || echo 'sshd=YES' >> /etc/rc.conf")
    run("grep -q '^PermitRootLogin' /etc/ssh/sshd_config || "
        "echo 'PermitRootLogin prohibit-password' >> /etc/ssh/sshd_config")
    run("grep -q '^PubkeyAuthentication yes' /etc/ssh/sshd_config || "
        "echo 'PubkeyAuthentication yes' >> /etc/ssh/sshd_config")
    run("/etc/rc.d/sshd restart 2>/dev/null || /etc/rc.d/sshd start", t=180)
    run("wc -l /root/.ssh/authorized_keys")
    run("sync")
    print("PROVISIONED_SSH", flush=True)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--con-port", type=int, required=True)
    ap.add_argument("--pubkey", default=os.path.expanduser("~/.ssh/veil_qemu_key.pub"))
    ap.add_argument("--password", default="veil")
    args = ap.parse_args()

    s = connect(args.con_port)
    child = fdpexpect.fdspawn(s.fileno(), encoding="latin-1", timeout=600)
    child.logfile_read = sys.stdout
    child.sendline("")

    try_switch_to_serial(child)
    login_and_inject_key(child, args.password, args.pubkey)
    s.close()


if __name__ == "__main__":
    main()
