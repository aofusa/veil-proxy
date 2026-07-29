#!/usr/bin/env python3
"""NetBSD aarch64 の sysinst をシリアルコンソール経由で自動操作する（F-140 / tools/qemu）。

NetBSD は aarch64 向けに **install ISO のみ**を配布しており（生イメージ無し）、
OpenBSD の `autoinstall(8)` のような応答ファイル方式が無いため、`sysinst`（メニュー
主導の対話型インストーラ）をシリアルへのキー送出で駆動する。

## 前提・注意（**重要: 実機コンソールで未検証**）

- qemu は `-serial telnet:0.0.0.0:<CON_PORT>,server,wait`（`CONSOLE_WAIT=1`）で
  起動していること。起動直後の efiboot/sysinst 出力を取り逃さないため。
- aarch64 は UEFI + efiboot でシリアルがそのまま使える想定
  （OpenBSD arm64 と同様。`tools/qemu/README.md` の教訓を踏襲）。
- **sysinst のメニュー文言・キー割り当ては NetBSD のバージョンで変わりうる。**
  本スクリプトはリリースノート・一般的な sysinst の操作手順（言語選択 →
  メインメニュー → "Install NetBSD to hard disk drive" → ディスク選択 →
  パーティション（Use one full disk with GPT 相当）→ セット取得元
  （CD-ROM/DVD、ISO 自体がセットを含む）→ 確認 → インストール → 完了後の
  終了操作）に基づく**最善の推定**であり、実機の文言に合わせて調整が要る。
  `tools/qemu/console-dump.py`（または本スクリプトの `--dump-only`）で実際の
  画面遷移を確認してから調整すること。
- **本スクリプトは sysinst の完了までしか行わない**（SSH 鍵注入・sshd 有効化は
  行わない）。理由: install フェーズは ISO を bootindex=0（最優先）で繋いだまま
  なので、sysinst 完了後にゲスト内から reboot すると **再び ISO から起動して
  インストーラへ戻ってしまう**。`bsd-vm.sh` の `cmd_provision` は本スクリプト
  完了後に一度 VM を落とし、**ISO を外した状態（通常の disk boot 構成）で
  up し直してから** `netbsd-provision.py`（x86_64 と共通）でシリアルログイン
  → SSH 鍵注入を行う。

失敗した場合は本スクリプトを打ち切って `tools/qemu/serial-exec.py` /
`console-dump.py` で手動デバッグすること。
"""
import argparse
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


def send_and_wait(child, text, patterns, timeout=300, sendline=True):
    if sendline:
        child.sendline(text)
    else:
        child.send(text)
    idx = child.expect(patterns + [TIMEOUT], timeout=timeout)
    if idx == len(patterns):
        sys.exit("TIMEOUT waiting for one of %r after sending %r" % (patterns, text))
    return idx


def run_sysinst(child, timeout: int) -> None:
    # 1. efiboot / ブートメニューでデフォルト（Install/Boot）を待つ。
    #    aarch64 の efiboot は既定でカウントダウン後に自動起動する想定。
    child.expect([r"Press.*to.*boot", r"boot>", TIMEOUT], timeout=180)
    time.sleep(2)
    child.sendline("")

    # 2. sysinst の言語選択（多くのリリースで最初に出る）。
    #    "a) English" のような選択肢を想定し、英語(a)を選ぶ。
    i = child.expect([r"English", r"sysinst", r"main menu", TIMEOUT], timeout=600)
    if i != 3:
        child.sendline("a")
        time.sleep(1)

    # 3. メインメニューから "Install NetBSD to hard disk drive" を選ぶ。
    #    メニュー項目には番号が振られているのが通例（環境により番号が変わりうる
    #    ため、まず文言でメニュー全体を待ってから "Install" を含む行の番号を選ぶ、
    #    という高度な自動化はここでは行わず、慣例上の番号 "a" または "1" を試す）。
    j = child.expect([r"[Ii]nstall NetBSD", TIMEOUT], timeout=180)
    if j == 0:
        child.sendline("a")
    time.sleep(1)

    # 4. ディスク選択（1 台しか無い前提で先頭を選択、Enter で確定）。
    child.expect([r"[Dd]isk", TIMEOUT], timeout=180)
    child.sendline("")
    time.sleep(1)

    # 5. パーティション方式: GPT / whole disk を選ぶ（"Use the entire disk" 相当）。
    child.expect([r"[Pp]artition", r"GPT", TIMEOUT], timeout=180)
    child.sendline("")
    time.sleep(1)

    # 6. 確認プロンプト（"This is your last chance..." 等）に yes。
    k = child.expect([r"[Ll]ast chance", r"[Aa]re you sure", r"[Yy]es", TIMEOUT], timeout=180)
    if k in (0, 1):
        child.sendline("x")  # "x: Yes"（sysinst の一般的な yes ショートカット）
    time.sleep(1)

    # 7. セット取得元: CD-ROM/DVD（ISO 自体がセットを含む）を選ぶ。
    m = child.expect([r"[Ss]ets", r"CD-ROM", TIMEOUT], timeout=300)
    if m != 2:
        child.sendline("")

    # 8. インストール完了（"Congratulations" 相当、または "installation complete"）。
    child.expect([r"[Cc]ongratulations", r"complete", TIMEOUT], timeout=timeout)
    print("\nNETBSD_SYSINST_DONE", flush=True)
    # ここで終了する。reboot はしない（ISO が bootindex=0 のままなので、reboot
    # すると再びインストーラへ戻ってしまう。`bsd-vm.sh` 側で ISO を外して up
    # し直してから `netbsd-provision.py` で SSH 鍵注入する）。


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--con-port", type=int, required=True)
    ap.add_argument("--pubkey", required=True, help="未使用（bsd-vm.sh との呼び出し互換のため残置）")
    ap.add_argument("--password", default="veil", help="未使用（同上）")
    ap.add_argument("--timeout", type=int, default=5400)
    args = ap.parse_args()

    s = connect(args.con_port)
    child = fdpexpect.fdspawn(s.fileno(), encoding="latin-1", timeout=args.timeout)
    child.logfile_read = sys.stdout

    run_sysinst(child, args.timeout)

    time.sleep(5)
    s.close()


if __name__ == "__main__":
    main()
