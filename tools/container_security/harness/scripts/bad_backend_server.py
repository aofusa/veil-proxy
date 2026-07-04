#!/usr/bin/env python3
"""プロトコル違反を返すモックバックエンド（カオステスト用）。

リクエストパスに応じて、意図的に不正な HTTP レスポンスを返す。
プロキシ（Veil）がこれらを安全に処理（502 応答 or 接続クローズ）し、
クラッシュ・無限ハング・バッファ破損を起こさないことを検証するために用いる。

依存なし（標準ライブラリのみ）。0.0.0.0:8080 で待受。
"""
import socket
import threading
import time

HOST, PORT = "0.0.0.0", 8080


def handle(conn: socket.socket) -> None:
    try:
        conn.settimeout(5)
        req = b""
        try:
            while b"\r\n\r\n" not in req:
                chunk = conn.recv(4096)
                if not chunk:
                    return
                req += chunk
        except socket.timeout:
            pass
        path = b"/"
        try:
            path = req.split(b" ", 2)[1]
        except Exception:
            pass

        if path.startswith(b"/truncated-headers"):
            # ヘッダー途中で切断
            conn.sendall(b"HTTP/1.1 200 OK\r\nContent-Type: text/pl")
        elif path.startswith(b"/cl-too-large"):
            # Content-Length > 実ボディ（クライアントは残りを待ってハングしうる）
            conn.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\nshort")
        elif path.startswith(b"/cl-too-small"):
            # Content-Length < 実ボディ（余剰バイト＝次応答への混線/スマグリング誘発）
            body = b"X" * 500
            conn.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n" + body)
        elif path.startswith(b"/huge-headers"):
            # 巨大ヘッダー（数百 KB）
            huge = b"A" * (256 * 1024)
            conn.sendall(b"HTTP/1.1 200 OK\r\nX-Huge: " + huge + b"\r\nContent-Length: 2\r\n\r\nok")
        elif path.startswith(b"/bad-status"):
            # 不正なステータスライン
            conn.sendall(b"HTTP/1.1 999 \x00\x01\x02\r\nContent-Length: 2\r\n\r\nok")
        elif path.startswith(b"/no-response"):
            # 受理後に無応答（読みタイムアウト検証）
            time.sleep(30)
        elif path.startswith(b"/instant-close"):
            # 何も返さず即クローズ
            pass
        else:
            conn.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
    except Exception:
        pass
    finally:
        try:
            conn.close()
        except Exception:
            pass


def main() -> None:
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind((HOST, PORT))
    s.listen(128)
    print(f"bad_backend listening on {HOST}:{PORT}", flush=True)
    while True:
        conn, _ = s.accept()
        threading.Thread(target=handle, args=(conn,), daemon=True).start()


if __name__ == "__main__":
    main()
