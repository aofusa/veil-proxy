#!/usr/bin/env python3
"""OpenBSD の autoinstall(8) をシリアルコンソール経由で駆動する（B-47 / tools/qemu）。

OpenBSD は FreeBSD と違い ready-made な VM イメージ（qcow2）を配布していないため、
`miniroot<NN>.img` から **無人インストール**して起動可能なディスクを作る。

## 全体の流れ

1. miniroot を 1 台目（ゲストから見て sd0）、空のターゲット qcow2 を 2 台目（sd1）
   として QEMU を起動しておく（`bsd-vm.sh openbsd <arch> provision` が実施）。
2. **amd64 のみ**: OpenBSD のブートローダは既定で VGA コンソールを使うので、
   `boot>` プロンプトへ `set tty com0` を送ってシリアルへ切り替える。
   SeaBIOS はコンソール出力をシリアルへミラーするので、`boot>` プロンプト自体は
   シリアルで**見える**（FreeBSD の検証で確認済み）。
   arm64 は UEFI + efiboot がファームウェアの ConOut を引き継ぐため不要。
3. インストーラの `(I)nstall, (U)pgrade, (A)utoinstall or (S)hell?` へ `A` を送る。
4. 応答ファイルの URL を渡す。slirp のゲートウェイ 10.0.2.2 は QEMU を実行している
   プロセス自身を指す。Docker モードでは helper コンテナ内に、native モード
   （`--container` 省略。`bsd-vm.sh` が `VEIL_QEMU_NATIVE=1` のときに使う）では
   ホスト上に直接 `python3 -m http.server` を立てて `auto_install.conf` を配る。
5. インストール完了（`CONGRATULATIONS`）まで待つ。

応答ファイルに書かれていない質問はインストーラの**既定値**が使われるため、
リリース間の質問追加にある程度強い（`bsd-vm.sh` の `_write_openbsd_autoinstall` 参照）。

## 前提

qemu を `-serial telnet:0.0.0.0:<CON_PORT>,server,nowait` で起動していること。
`nowait` は接続前の出力を捨てるので、**VM 起動直後に**本スクリプトを走らせること。
telnet の IAC(0xff) が混ざるため pexpect の encoding は latin-1。

## 注意

本スクリプトは OpenBSD インストーラの対話文言に依存する。リリースで文言が変わった
場合はここを追従させる必要がある。
"""
import argparse
import os
import socket
import subprocess
import sys
import time

from pexpect import fdpexpect, TIMEOUT  # type: ignore

HTTP_PORT = 8000


def connect(con_port: int, timeout: int = 180):
    """コンソール（telnet）へ接続する。qemu 起動直後は数秒間 listen していない。"""
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


def start_http_server_container(container: str) -> None:
    """helper コンテナ内で auto_install.conf を配る HTTP サーバを起動する（Docker モード）。

    QEMU の slirp では 10.0.2.2 がゲートウェイ（= QEMU プロセスのネットワーク空間）
    なので、ゲストからは http://10.0.2.2:8000/auto_install.conf で取得できる。
    """
    subprocess.run(
        ["docker", "exec", "-d", container, "sh", "-c",
         "cd /w && python3 -m http.server %d >/w/httpd.log 2>&1" % HTTP_PORT],
        check=True,
    )
    time.sleep(2)


def start_http_server_native(workdir: str):
    """Docker を使わず、ホスト上で直接 auto_install.conf を配る（native モード）。

    QEMU 自体をホストで直接起動している（helper コンテナが無い）ので、slirp の
    ゲートウェイ 10.0.2.2 は QEMU プロセス自身＝ホストを指す。ホストで
    `python3 -m http.server` を 0.0.0.0:8000 にバインドして起動すればよい。
    呼び出し元は Popen を保持し、インストール完了後に terminate() すること。
    """
    log = open(os.path.join(workdir, "httpd.log"), "wb")
    proc = subprocess.Popen(
        [sys.executable, "-m", "http.server", str(HTTP_PORT), "--bind", "0.0.0.0"],
        cwd=workdir,
        stdout=log,
        stderr=subprocess.STDOUT,
    )
    time.sleep(2)
    return proc


def switch_to_serial(child, arch: str) -> None:
    """amd64 のブートローダをシリアルコンソールへ切り替える。"""
    if arch != "x86_64":
        return
    # SeaBIOS 経由でブートローダの出力はシリアルに見える。`boot>` を待って切り替える。
    i = child.expect([r"boot>", TIMEOUT], timeout=180)
    if i == 1:
        # プロンプトを取り逃した場合に備えてブラインドでも送る（無害）。
        print("\n(did not see 'boot>'; sending blind)", flush=True)
    child.sendline("set tty com0")
    time.sleep(1.5)
    # コンソール切り替え直後は最初の 1 文字が落ちることがあるので空行を挟む
    child.sendline("")
    time.sleep(1.0)
    child.sendline("boot")


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--con-port", type=int, required=True)
    ap.add_argument("--workdir", required=True)
    ap.add_argument(
        "--container", default=None,
        help="helper コンテナ名（Docker モード）。省略時は native モード"
             "（Docker を使わず、ホスト上で直接 python3 -m http.server を起動する）",
    )
    ap.add_argument("--arch", choices=["x86_64", "aarch64"], required=True)
    ap.add_argument("--timeout", type=int, default=5400)
    args = ap.parse_args()

    http_proc = None
    if args.container:
        start_http_server_container(args.container)
    else:
        http_proc = start_http_server_native(args.workdir)

    try:
        s = connect(args.con_port)
        child = fdpexpect.fdspawn(s.fileno(), encoding="latin-1", timeout=args.timeout)
        child.logfile_read = sys.stdout

        switch_to_serial(child, args.arch)

        i = child.expect(
            [r"\(I\)nstall, \(U\)pgrade, \(A\)utoinstall or \(S\)hell", TIMEOUT],
            timeout=1200,
        )
        if i == 1:
            sys.exit("TIMEOUT waiting for the installer prompt")
        time.sleep(1)
        child.sendline("A")

        # 応答ファイルの場所を聞かれる（DHCP で得られなかった場合）。
        j = child.expect(
            [r"[Rr]esponse file location", r"CONGRATULATIONS", TIMEOUT], timeout=900
        )
        if j == 0:
            child.sendline("http://10.0.2.2:%d/auto_install.conf" % HTTP_PORT)
        elif j == 2:
            sys.exit("TIMEOUT waiting for the response file prompt")

        k = child.expect([r"CONGRATULATIONS", TIMEOUT], timeout=args.timeout)
        if k == 1:
            sys.exit("TIMEOUT waiting for the install to finish")
        print("\nOPENBSD_AUTOINSTALL_DONE", flush=True)
        time.sleep(15)
        s.close()
    finally:
        if http_proc is not None:
            http_proc.terminate()
            try:
                http_proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                http_proc.kill()


if __name__ == "__main__":
    main()
