# F-311: HTTP/3 の受信で空のソケットに並べる待ちの RECVMSG を 1 本にする

**状態: 計測中（feat/beat-nginx、2026-11-14）**

## 背景

CPU を専有させた配置（lab50）でも、HTTP/3 の 1 接続 1 ストリーム（`-m1`）で nginx を下回っていた（静的配信 −5%・中継 −11%）。
負荷側（h2load-h3）の CPU の内訳を perf で採ると（lab52）、veil が相手のときだけ、負荷側のカーネルで `io_poll_wake`・
`llist_add_batch`・`__list_del_entry_valid_or_report` が合わせて約 6% を占めていた（nginx が相手のときの `ep_poll_callback` は 0.4%）。

io_uring の受信（F-130 C1、`PipelinedUdpRecv`）は、`mmsg_batch_size`（既定 64）本の `IORING_OP_RECVMSG` をすべて
`IORING_RECVSEND_POLL_FIRST` で積んでいた。空のソケットでは全部の op が内部 poll の待ちに並ぶ。RECVMSG の内部 poll の
待ちは排他でない（`EPOLLEXCLUSIVE` を付けるのは ACCEPT だけ）ので、データグラムが 1 つ届くたびに並んだ 64 本がすべて
起こされ、1 本がデータグラムを受け取り、残りの 63 本は空振りして待ちに並び直す。起こす処理（待ちから外して task work を
積む）はデータグラムを送った側のコンテキスト（ループバック・veth では送り手の CPU）で走るので、相手の CPU も使わせていた。

待ちの本数を変えて測った（lab54、aofish のコンテナ間、専有の配置、h2load-h3 `-t4 -c100`、54KB、2 ラウンドの中央値。
CPU はサーバの µs/要求）:

| 構成 | nginx | 64 本（既定） | 8 本 | 1 本 | multishot（`VEIL_H3_BUFRING=1`） |
|---|---:|---:|---:|---:|---:|
| `-m1` 静的配信 rps | 40,448 | 39,706 | 40,395 | 42,540 | 42,152 |
| 　サーバの CPU/要求 | 173.6 | 161.9 | 157.2 | 155.4 | 148.8 |
| `-m10` 静的配信 rps | 44,598 | 46,836 | 50,454 | 48,319 | 49,879 |
| 　サーバの CPU/要求 | 163.2 | 144.9 | 131.2 | 139.6 | 129.4 |
| `-m1` 中継 rps | 29,246 | 26,824 | 30,266 | 27,770 | 30,084 |
| 　サーバの CPU/要求 | 245.5 | 265.2 | 232.9 | 243.1 | 225.9 |

`mmsg_batch_size` を小さくすると待ちは減るが、溜まったデータグラムを 1 周回で受けられる数も減る（AGENTS.md の
「最重要チューニング項目」）。multishot（F-130 C2）は 1 本の op で待ち続けるので同じ問題は無いが、開発機のカーネル
（6.8.0）が `IORING_REGISTER_PBUF_RING` を断るので既定にはしない（aofish の Docker Desktop のカーネルでは動いた）。

## 対応

- 空のソケットで待つ RECVMSG（`IORING_RECVSEND_POLL_FIRST`）は常に 1 本だけにする。
- 溜まったデータグラムは `MSG_DONTWAIT` の RECVMSG（掻き出し）で受ける。io_uring は `MSG_DONTWAIT` の op を内部 poll に
  並べず、受け取るものが無ければ `-EAGAIN` で完了させる（`REQ_F_NOWAIT`）。掻き出しは提出の中でその場で完了する。
- 掻き出しの本数は直近の結果で決める: 全部が受け取れたら倍、空振りがあったら受け取れた本数、掻き出しを積んでおらず
  待ちが受け取ったら 1 本だけ試す。上限は `mmsg_batch_size - 1`。溜まっている間は従来と同じ幅で受ける。
- 掻き出しの空振りは受信の結果に数えない（`scan`）。待ちが無いのに完了も無いとき（積み直しの失敗）は、`recv_batch` の
  中で待ちを 1 本積み直す。
- 新しいオペコード・SQE フラグは使わない（`MSG_DONTWAIT` は RECVMSG の `msg_flags`）。reactor（epoll・kqueue）と
  multishot（C2）は変えない。

## 計測

lab55（同じバイナリで旧方式に戻す一時的なビルド。標準と専有の配置で nginx と交互に 3 ラウンド）で測る。

## テスト

- `runtime::uring::udp_recv::f311_tests::f311_one_waiter_and_burst_is_drained`（1 つずつ届くデータグラムを毎回受け、待ちは
  常に 1 本。まとめて届いた 100 個を全部受け、掻き出しの本数が 16 本以上に増える。io_uring を使えない環境では飛ばす）。
- 機能 × プロトコルのマトリックス（io_uring）と HTTP/3 の E2E。
