# B-310: Windows の HTTP/3（と L4 の UDP）の受信が、閉じたクライアントへの送信の ICMP で `WSAECONNRESET` になり、受信の周回を捨ててエラーをログする

**状態: 修正済み（v0.11.0）**

## 事象

Windows（aofish）の E2E で、プロキシのログに `[HTTP/3] recv_gro error: 既存の接続はリモート ホストに強制的に切断されました。 (os error 10054)`
が ERROR で大量に出た（試験のクライアントが UDP のソケットを閉じるたび）。

## 原因

Windows の UDP ソケットは既定で、送ったデータグラムへの ICMP port unreachable を、次の `recv`/`recvfrom` の
`WSAECONNRESET` として返す（`SIO_UDP_CONNRESET`）。HTTP/3 のメインループはそれを受信のエラーとして ERROR でログし、
その周回の受信を捨てる（キューにある他の接続のデータグラムの処理が 1 周回遅れる）。閉じたクライアントへ再送・
CONNECTION_CLOSE を送るたびに起きる（クライアントの数だけ、要求ごとに近い頻度で ERROR が出る）。

## 修正

`runtime::udp::disable_udp_connreset`（`WSAIoctl(SIO_UDP_CONNRESET, FALSE)`）を Windows の UDP ソケットの作成時に呼ぶ
（HTTP/3 の `QuicUdpSocket::bind_reuseport`、L4 の `UdpSocket`）。quinn・tokio の利用者と同じ対処。

## 関連

- B-311（Windows の E2E）
