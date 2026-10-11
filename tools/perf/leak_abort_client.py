#!/usr/bin/env python3
"""leak_abort_client.py — 異常切断の接続を大量に作る（F-185、leak_check.sh の abort シナリオ）。

正常な負荷ツール（h2load・wrk）は要求を最後まで読み、接続を FIN で閉じる。リークが
起きやすいのはその逆の経路（ハンドシェイク途中・要求途中・応答途中の切断、RST）なので、
次の種類を混ぜて N 回ずつ実行する（標準ライブラリだけで動く）:

  tcp_close     TCP 接続だけして即 close（TLS のハンドシェイク前）
  tls_partial   ClientHello の途中まで送って RST
  h1_partial    TLS ハンドシェイク後、要求ヘッダの途中まで送って RST
  h1_midresp    TLS で 54KB を要求し、応答の先頭だけ読んで RST
  h2_midresp    TLS + ALPN h2 で GET を送り、応答の先頭だけ読んで RST
  h2c_midresp   h2c（平文 HTTP/2）でプロキシ経路を要求し、応答の先頭だけ読んで RST

使い方: leak_abort_client.py <tls_port> <h2c_port> <回数/種類> [並列数]
終了コード 0。種類ごとの「サーバが応答を返し始めた回数」を標準出力に出す（0 なら
その種類は目的の経路まで届いていない＝測れていない）。
"""
import socket
import ssl
import struct
import sys
import threading

HOST = "127.0.0.1"


def rst_close(s):
    # SO_LINGER 0 で close すると FIN ではなく RST を送る
    try:
        s.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
    except OSError:
        pass
    s.close()


def tls_ctx(alpn):
    c = ssl.create_default_context()
    c.check_hostname = False
    c.verify_mode = ssl.CERT_NONE
    c.set_alpn_protocols([alpn])
    return c


CTX_H1 = tls_ctx("http/1.1")
CTX_H2 = tls_ctx("h2")


def h2_frame(ftype, flags, sid, payload):
    return struct.pack(">I", len(payload))[1:] + bytes([ftype, flags]) + struct.pack(">I", sid) + payload


def h2_get(scheme_https, path):
    # HPACK: :method GET (2) / :scheme https (7) or http (6) / :path（名前 4 の literal）/ :authority（名前 1）
    hb = bytes([0x82, 0x87 if scheme_https else 0x86])
    hb += bytes([0x04, len(path)]) + path
    auth = b"127.0.0.1"
    hb += bytes([0x01, len(auth)]) + auth
    preface = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n"
    return preface + h2_frame(4, 0, 0, b"") + h2_frame(1, 0x05, 1, hb)


def tcp_close(tls_port, _h2c):
    s = socket.create_connection((HOST, tls_port), timeout=5)
    s.close()
    return False


def tls_partial(tls_port, _h2c):
    s = socket.create_connection((HOST, tls_port), timeout=5)
    # TLS レコードヘッダ（handshake, TLS1.0, 長さ 512）+ ClientHello の先頭だけ
    s.sendall(b"\x16\x03\x01\x02\x00\x01\x00\x01\xfc\x03\x03" + b"\x00" * 40)
    rst_close(s)
    return False


def h1_partial(tls_port, _h2c):
    raw = socket.create_connection((HOST, tls_port), timeout=5)
    s = CTX_H1.wrap_socket(raw, server_hostname="localhost")
    s.sendall(b"GET /3b.html HTTP/1.1\r\nHost: 127.0.0.1\r\nX-Partial: ")
    rst_close(s)
    return True


def h1_midresp(tls_port, _h2c):
    raw = socket.create_connection((HOST, tls_port), timeout=5)
    s = CTX_H1.wrap_socket(raw, server_hostname="localhost")
    s.sendall(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
    got = s.recv(1024)
    rst_close(s)
    return got.startswith(b"HTTP/1.1 200")


def h2_midresp(tls_port, _h2c):
    raw = socket.create_connection((HOST, tls_port), timeout=5)
    s = CTX_H2.wrap_socket(raw, server_hostname="localhost")
    if s.selected_alpn_protocol() != "h2":
        rst_close(s)
        return False
    s.sendall(h2_get(True, b"/"))
    got = recv_until_headers(s)
    rst_close(s)
    return got


def h2c_midresp(_tls, h2c_port):
    s = socket.create_connection((HOST, h2c_port), timeout=5)
    s.sendall(h2_get(False, b"/p/"))
    got = recv_until_headers(s)
    rst_close(s)
    return got


def recv_until_headers(s):
    # HEADERS フレーム（type 1）を受け取るまで読む（SETTINGS 等は読み飛ばす）
    buf = b""
    for _ in range(32):
        d = s.recv(4096)
        if not d:
            return False
        buf += d
        while len(buf) >= 9:
            ln = int.from_bytes(buf[0:3], "big")
            if buf[3] == 1:
                return True
            if len(buf) < 9 + ln:
                break
            buf = buf[9 + ln:]
    return False


KINDS = [tcp_close, tls_partial, h1_partial, h1_midresp, h2_midresp, h2c_midresp]


def main():
    tls_port, h2c_port, n = int(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3])
    par = int(sys.argv[4]) if len(sys.argv) > 4 else 16
    jobs = [k for k in KINDS for _ in range(n)]
    lock = threading.Lock()
    ok = {k.__name__: 0 for k in KINDS}
    err = {k.__name__: 0 for k in KINDS}

    def worker(i):
        for j in range(i, len(jobs), par):
            k = jobs[j]
            try:
                r = k(tls_port, h2c_port)
                with lock:
                    if r:
                        ok[k.__name__] += 1
            except (OSError, ssl.SSLError):
                with lock:
                    err[k.__name__] += 1

    ts = [threading.Thread(target=worker, args=(i,)) for i in range(par)]
    for t in ts:
        t.start()
    for t in ts:
        t.join()
    print(" ".join(f"{k}={ok[k]}/{err[k]}" for k in ok))


if __name__ == "__main__":
    main()
