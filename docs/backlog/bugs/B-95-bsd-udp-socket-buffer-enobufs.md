# B-95: BSD で HTTP/3 の UDP ソケットバッファ拡大が ENOBUFS で失敗し、既定の 42KB のまま動いていた

## 事象

FreeBSD aarch64 の h3 計測で、クライアント送信の約 14% が失われていた（quinn の
`lost_packets` 1244 / `sent_packets` 9163、輻輳イベント 315 回）。`netstat -s -p udp` の
`dropped due to full socket buffers` は数千万件。

## 原因

非 Linux の `QuicUdpSocket::bind_reuseport` は `SO_RCVBUF`/`SO_SNDBUF` に 2MB を要求し、
失敗を無視していた。Linux は `net.core.rmem_max` で黙って切り詰めるだけだが、**BSD は上限を
超えると `ENOBUFS` で失敗し、バッファは OS 既定のまま残る**。FreeBSD の上限は
`kern.ipc.maxsockbuf`（既定 2MB）から mbuf のオーバーヘッドを差し引いた値なので、2MB ちょうどの
要求は既定設定で**必ず**失敗していた（`truss` で `ERR#55 'No buffer space available'` を確認）。
受信バッファは `net.inet.udp.recvspace`（既定 42080 バイト）のままだった。

## 修正

`set_socket_buffer_best_effort`: 2MB から 1/8 ずつ下げて通る最大値を設定する（下限 64KB。NetBSD の既定 `kern.sbmax` は 256KB で実効上限が約 230KB のため、当初の下限 256KB では NetBSD で一度も設定できなかった。
起動時 1 回のコールドパス）。下限でも通らなければ警告ログを出す。Linux 経路は無変更。

計測ツール `tools/perf/h3load` も同じ問題を抱えていた（quinn の既定ソケット）ため、
クライアント側でも同じ方式で送受信バッファを広げた。負荷ツール側の取りこぼしは
サーバの損失回復（PTO の指数バックオフ）として計測に乗り、**1 接続が 30 秒止まって
アイドルタイムアウトになる**（計測が 10 秒のはずが 35〜39 秒かかる）現象として現れていた。
veil は 1 リクエストあたりの送信パケットが nginx より多いぶん影響を受けやすかった。

## テスト

`udp::socket::tests::test_set_socket_buffer_best_effort_enlarges_rcvbuf`（非 Linux の unix のみ）。

## 結果（FreeBSD 14.3 aarch64、3B、h3load `-c64 -m32`）

veil・nginx 交互 8 ラウンドでストール 0・エラー 0。対 nginx 比の中央値は約 0.89。
