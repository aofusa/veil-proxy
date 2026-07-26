#!/usr/bin/env python3
"""QEMU の QMP `send-key` でゲストへキー入力を送る（B-47 / tools/qemu）。

用途: FreeBSD amd64 は**シリアルへ一切出力しない**（ローダもカーネルも VGA コンソール）
ため、`bsd-vm.sh provision` のシリアル操作が効かない。そこで VGA コンソールへ
**ブラインドでキーを送り**、ローダプロンプトで `console="comconsole"` を設定してから
起動することでシリアルを有効化する。以降は通常どおり pexpect でシリアルを扱える。

（同種の手法は本リポジトリの過去セッションでも使われていた。ここではそれを
tools/qemu の一部として再利用可能な形に整理している。）

使い方:
  # ローダプロンプトへ入り、シリアルを有効化して single-user で起動する
  python3 qmp-sendkeys.py --port 2312 --freebsd-serial-boot

  # 任意の文字列を送る（末尾で Enter）
  python3 qmp-sendkeys.py --port 2312 --type 'boot -s'

  # 個別キー / 待機
  python3 qmp-sendkeys.py --port 2312 --key 3 --sleep 2 --type 'boot'

注意: 画面が見えないブラインド操作なので、待ち時間はゲストの起動速度に依存する。
"""
import argparse
import json
import socket
import sys
import time

# シフトを伴わない記号 → qcode
PLAIN = {
    ".": "dot", "/": "slash", "-": "minus", ";": "semicolon", " ": "spc",
    "=": "equal", ",": "comma", "'": "apostrophe", "[": "bracket_left",
    "]": "bracket_right", "\\": "backslash", "`": "grave_accent",
}
# シフトを伴う記号 → (shift + qcode)
SHIFT = {
    ":": "semicolon", "_": "minus", "&": "7", "!": "1", "@": "2", "#": "3",
    "$": "4", "%": "5", "^": "6", "*": "8", "(": "9", ")": "0",
    "~": "grave_accent", '"': "apostrophe", "<": "comma", ">": "dot",
    "?": "slash", "+": "equal", "{": "bracket_left", "}": "bracket_right",
    "|": "backslash",
}


def qcodes(ch: str):
    if ch.isalpha():
        return ["shift", ch.lower()] if ch.isupper() else [ch]
    if ch.isdigit():
        return [ch]
    if ch in PLAIN:
        return [PLAIN[ch]]
    if ch in SHIFT:
        return ["shift", SHIFT[ch]]
    raise ValueError("no qcode mapping for %r" % ch)


class Qmp:
    def __init__(self, port: int, timeout: int = 120):
        self.sock = socket.socket()
        deadline = time.time() + timeout
        while True:
            try:
                self.sock.connect(("127.0.0.1", port))
                break
            except OSError:
                if time.time() > deadline:
                    sys.exit("cannot connect to QMP on 127.0.0.1:%d" % port)
                time.sleep(2)
        # QMP を telnet で公開しているため、先頭に telnet の IAC(0xff) ネゴシエーションが
        # 混ざる。UTF-8 では復号できないので latin-1 で読み、JSON の開始位置（'{'）まで
        # 読み飛ばす。
        self.f = self.sock.makefile("rw", encoding="latin-1", newline="\n")
        self._readline_json()  # greeting
        self.cmd({"execute": "qmp_capabilities"})

    def _readline_json(self):
        """1 行読み、JSON として解釈できるまで先頭のゴミ（IAC 等）を捨てる。"""
        while True:
            line = self.f.readline()
            if not line:
                sys.exit("QMP connection closed")
            start = line.find("{")
            if start < 0:
                continue
            try:
                return json.loads(line[start:])
            except json.JSONDecodeError:
                continue

    def cmd(self, obj):
        self.f.write(json.dumps(obj) + "\n")
        self.f.flush()
        while True:
            d = self._readline_json()
            if "return" in d or "error" in d:
                return d

    def send_keys(self, keys):
        self.cmd({
            "execute": "send-key",
            "arguments": {"keys": [{"type": "qcode", "data": k} for k in keys]},
        })

    def type_text(self, text: str, enter: bool = True, delay: float = 0.06):
        for ch in text:
            self.send_keys(qcodes(ch))
            time.sleep(delay)
        if enter:
            self.send_keys(["ret"])
            time.sleep(0.4)


def freebsd_serial_boot(q: Qmp, menu_wait: float, single_user: bool) -> None:
    """FreeBSD のローダメニューからシリアルコンソールを有効化して起動する。

    beastie メニューの「3」= Escape to loader prompt。そこで
    `set console="comconsole"` を入れてから `boot`（`boot -s` で single-user）する。
    """
    print("waiting %.1fs for the FreeBSD loader menu ..." % menu_wait, flush=True)
    time.sleep(menu_wait)
    print("sending '3' (Escape to loader prompt)", flush=True)
    q.send_keys(["3"])
    time.sleep(2)
    print('typing: set console="comconsole"', flush=True)
    q.type_text('set console="comconsole"')
    time.sleep(1)
    cmd = "boot -s" if single_user else "boot"
    print("typing: %s" % cmd, flush=True)
    q.type_text(cmd)
    print("FREEBSD_SERIAL_BOOT_SENT", flush=True)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, required=True, help="QMP telnet port")
    ap.add_argument("--freebsd-serial-boot", action="store_true",
                    help="FreeBSD ローダでシリアルを有効化して起動する")
    ap.add_argument("--single-user", action="store_true",
                    help="--freebsd-serial-boot 時に `boot -s` で single-user 起動する")
    ap.add_argument("--menu-wait", type=float, default=8.0,
                    help="ローダメニューが出るまでの待ち秒数（既定 8）")
    ap.add_argument("--key", action="append", default=[], help="送るキー（qcode）")
    ap.add_argument("--type", dest="text", action="append", default=[],
                    help="タイプする文字列（末尾で Enter）")
    ap.add_argument("--sleep", type=float, default=0.0, help="処理前の待機秒数")
    ap.add_argument("--powerdown", action="store_true",
                    help="ACPI シャットダウン（system_powerdown）を送る")
    args = ap.parse_args()

    if args.sleep:
        time.sleep(args.sleep)

    q = Qmp(args.port)
    if args.powerdown:
        q.cmd({"execute": "system_powerdown"})
        print("POWERDOWN_SENT", flush=True)
        return
    if args.freebsd_serial_boot:
        freebsd_serial_boot(q, args.menu_wait, args.single_user)
    for k in args.key:
        q.send_keys([k])
        time.sleep(0.5)
    for t in args.text:
        q.type_text(t)


if __name__ == "__main__":
    main()
