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

    実機コンソール（B-55 調査で確認済み）では、カーネルコンソールが既定で VGA
    （`ttyE0`）に向いており、ブートローダのメニューだけがシリアルにも出力される:

        NetBSD/x86 ffsv1 Primary Bootstrap
          1. Boot normally
          2. Boot single user
          3. Drop to boot prompt
        Choose an option; RETURN for default; SPACE to stop countdown.

    カウントダウンは既定 5 秒しかなく、`>>` のようなブートプロンプトはこの時点
    ではまだ出ない（"3. Drop to boot prompt" を選んで初めて `>` プロンプトに
    落ちる）。そのため:
      1. "Choose an option" を検出したら、まず SPACE でカウントダウンを止める
         （5 秒の猶予をあてにせず、確実に操作できる状態にする）。
      2. "3" を送って boot prompt へ落ちる。
      3. boot prompt で `consdev com0` を送りシリアルへ切り替え、`boot` で続行。

    メニューが見えない（＝既にシリアルへ出ている、または速すぎて通過した）
    場合は何もせず戻る。失敗しても致命的ではない設計にしてある
    （その場合ログイン待ちがタイムアウトし、呼び出し元で気付ける）。
    """
    i = child.expect([r"Choose an option", r">\s*$", r"login:", r"Last message repeated", TIMEOUT],
                      timeout=90)
    if i == 0:
        print("\n(boot menu 'Choose an option' detected; stopping countdown and selecting"
              " '3. Drop to boot prompt')", flush=True)
        child.send(" ")  # SPACE: カウントダウン停止（5 秒の猶予に賭けない）
        # SPACE でカウントダウンを止めると "Option: [1]:" という行入力プロンプトに
        # なる（実測: 数字キー即時選択ではなく、Enter で確定するテキスト入力）。
        child.expect([r"Option:\s*\[1\]:", TIMEOUT], timeout=10)
        child.sendline("3")
        j = child.expect([r">\s*$", TIMEOUT], timeout=30)
        if j == 0:
            print("\n(boot prompt '>' detected; sending 'consdev com0' then 'boot')", flush=True)
            child.sendline("consdev com0")
            time.sleep(1.0)
            child.sendline("boot")
        else:
            print("\n(boot prompt not reached after selecting option 3; continuing anyway)",
                  flush=True)
    elif i == 1:
        print("\n(boot prompt '>' detected directly; trying 'consdev com0')", flush=True)
        child.sendline("consdev com0")
        time.sleep(1.0)
        child.sendline("boot")
    elif i == 2:
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

    # 恒久化: 次回以降の起動でブートメニュー操作（3 を選んで consdev com0 を打つ）
    # をせずに済むよう、/boot.cfg の先頭へ `consdev=com0` を書き込む。
    # boot.cfg(5) のグローバルディレクティブで、ブートローダ自身の表示と
    # カーネルへ渡す consdev の双方に効く（`/etc/ttys` の `console` エントリは
    # カーネルコンソールに追従するため、これだけでシリアルに getty が出る）。
    # 既存の consdev= 行は除去してから先頭に差し込む（冪等・再実行安全）。
    run("test -f /boot.cfg && cp /boot.cfg /boot.cfg.orig 2>/dev/null; "
        "{ echo 'consdev=com0'; grep -v '^consdev=' /boot.cfg 2>/dev/null; } > /boot.cfg.new && "
        "mv /boot.cfg.new /boot.cfg")
    run("cat /boot.cfg")
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
