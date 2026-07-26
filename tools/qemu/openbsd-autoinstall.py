#!/usr/bin/env python3
"""OpenBSD の autoinstall(8) をシリアルコンソール経由で駆動する（B-47）。

OpenBSD は FreeBSD と違い ready-made な VM-IMAGE（qcow2）を配布していないため、
`installNN.img`（miniroot）から **無人インストール**して起動可能なディスクを作る。

流れ:
  1. miniroot を 1 台目、空のターゲット qcow2 を 2 台目として QEMU を起動しておく
     （呼び出し元 `bsd-vm.sh <os> <arch> provision` が実施）。
  2. amd64 はブートローダが既定で VGA コンソールへ出るため、`boot>` プロンプトへ
     **ブラインドで** `set tty com0` を送ってシリアルへ切り替える
     （arm64 は UEFI + efiboot が既定でシリアルなので不要）。
  3. インストーラの `(I)nstall, (U)pgrade, (A)utoinstall or (S)hell?` へ `A` を送り、
     応答ファイルの URL を渡す。slirp のゲートウェイ 10.0.2.2 は QEMU を実行している
     helper コンテナ自身なので、同コンテナ内に `python3 -m http.server` を立てて
     `auto_install.conf` を配る。
  4. インストール完了（`CONGRATULATIONS` / `rebooting`）まで待つ。

前提: qemu を `-serial telnet:0.0.0.0:<CON_PORT>,server,nowait` で起動していること。
      telnet の IAC(0xff) が混ざるため pexpect の encoding は latin-1。

注意: 本スクリプトは OpenBSD インストーラの対話文言に依存する。OpenBSD の
      リリースによって文言が変わった場合はここを追従させる必要がある。
"""
import argparse
import socket
import subprocess
import sys
import time

from pexpect import fdpexpect, TIMEOUT  # type: ignore

HTTP_PORT = 8000


def connect(con_port: int, timeout: int = 120):
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            s.connect(("127.0.0.1", con_port))
            return s
        except OSError:
            time.sleep(2)
    sys.exit("cannot connect to console TCP %d" % con_port)


def start_http_server(container: str) -> None:
    """helper コンテナ内で auto_install.conf を配る HTTP サーバを起動する。

    QEMU の slirp では 10.0.2.2 がゲートウェイ（= QEMU プロセスのネットワーク名前空間）
    なので、ゲストからは http://10.0.2.2:8000/auto_install.conf で取得できる。
    """
    subprocess.run(
        ["docker", "exec", "-d", container, "sh", "-c",
         "cd /w && python3 -m http.server %d >/w/httpd.log 2>&1" % HTTP_PORT],
        check=True,
    )
    time.sleep(2)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--con-port", type=int, required=True)
    ap.add_argument("--workdir", required=True)
    ap.add_argument("--container", required=True)
    ap.add_argument("--arch", choices=["x86_64", "aarch64"], required=True)
    ap.add_argument("--timeout", type=int, default=3600)
    args = ap.parse_args()

    start_http_server(args.container)

    s = connect(args.con_port)
    child = fdpexpect.fdspawn(s.fileno(), encoding="latin-1", timeout=args.timeout)
    child.logfile_read = sys.stdout

    if args.arch == "x86_64":
        # amd64 のブートローダは既定で VGA。`boot>` は見えないのでブラインド送信する。
        time.sleep(5)
        child.send("set tty com0\r\n")
        time.sleep(2)
        child.send("boot\r\n")

    i = child.expect([r"\(I\)nstall, \(U\)pgrade, \(A\)utoinstall or \(S\)hell\?", TIMEOUT],
                     timeout=900)
    if i == 1:
        sys.exit("TIMEOUT waiting for installer prompt")
    child.send("A\r\n")

    # 応答ファイルの場所を聞かれる（DHCP で得られなかった場合）。
    j = child.expect([r"Response file location\?", r"CONGRATULATIONS", TIMEOUT], timeout=600)
    if j == 0:
        child.send("http://10.0.2.2:%d/auto_install.conf\r\n" % HTTP_PORT)
    elif j == 2:
        sys.exit("TIMEOUT waiting for response file prompt")

    k = child.expect([r"CONGRATULATIONS", r"rebooting", TIMEOUT], timeout=args.timeout)
    if k == 2:
        sys.exit("TIMEOUT waiting for install completion")
    print("\nOPENBSD_AUTOINSTALL_DONE", flush=True)
    time.sleep(10)
    s.close()


if __name__ == "__main__":
    main()
