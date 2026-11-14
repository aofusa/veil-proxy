# F-305: HTTP/3 の送信の GSO を既定で有効にし、送れない経路では自動で下ろす

**状態: 完了（feat/beat-nginx、2026-11-14）**

## 背景

コンテナ間の HTTP/3（h2load の `--alpn-list=h3 -t4 -c100 -m10`、54KB の静的配信・中継）で、veil は nginx と同程度
（GSO 無し同士）だった。負荷側の h2load は 4 スレッドでほぼ飽和しており（CPU 300〜390%）、rps は負荷側が 1 要求に
使う CPU で決まる。

- `[http3] gso_gro_enabled` は既定 false だった（以前の計測（F-89、HTTP/3 が 400 rps の頃）で Docker では効果が
  無かったため）。現在の送信経路（F-115・F-130・F-151）で有効にすると、veil のサーバの CPU は 1 要求あたり
  約 340µs → 150µs（カーネル 742k → 177k サイクル）、負荷側も GRO でまとめて受け取れて 1 要求あたり約 120µs → 61µs
  になり、rps は 21〜23k → 36k になった（lab12）。
- nginx も `quic_gso on`（既定は off）で 22〜25k → 41〜45k になる（lab13）。比較は両方 GSO 有りで行う
  （`tools/perf/nginx/nginx.conf` に `quic_gso on`）。
- 両方 GSO 有りでは veil が約 2 割遅かった。原因は [B-361](../bugs/B-361-h3-gso-socket-default-splits-single-packets.md)
  （GSO を有効にすると経路 MTU の探索が必ず失敗し、パケットが 1200 バイトに留まる。nginx は 1452 バイト）。

## 対応

1. `[http3] gso_gro_enabled` の既定を true にする（Linux のみ。他の OS では従来どおり無視）。
2. 送れない経路では自動で下ろす。カーネルは次の場合に GSO の束（`UDP_SEGMENT` の cmsg 付きの送信）を拒む:
   - `EIO`: デバイスがチェックサムのオフロードを持たない（`CHECKSUM_PARTIAL` にならない）、IPsec（xfrm）の経路
   - `EINVAL`: チェックサムを付けないソケット（`SO_NO_CHECK`）等

   送出（io_uring の `IORING_OP_SENDMSG`・reactor の `sendmmsg`）が束（`segments > 1`）でこれらを返したら、
   完了の値に添字を返す（`GsoRejected`。1 回の送出は `MMSG_BATCH_MAX` = 128 件までなので u128 のビット）。
   `send_mmsg_flush` はソケットの GSO を下ろし（`QuicUdpSocket::disable_gso`。以後の周回は束を作らない）、
   拒まれた束を 1 パケットずつのエントリに分けて送り直す（パケットを失わない）。warn はプロセスで 1 回だけ出す。
   ソケットにつき 1 回だけ通る冷たい経路なので、分けたエントリの `Vec` を確保してよい。
   `EMSGSIZE`（経路 MTU を超える。探索のパケット）は GSO と関係なく起きるので下ろさない。
3. 計測の nginx の設定に `quic_gso on` を足す（同じ条件で比べる）。

ホットパスへの追加は、送出の完了ごとの「束か・拒否の errno か」の比較（失敗した送出のときだけ）と、
`gso_enabled` の `Cell` の読み出し（周回に 1 回）だけ。

## テスト

- 単体: `gso_rejection_falls_back_to_single_packets`（`SO_NO_CHECK` を付けたソケットで 3 パケットの束を送ると、
  GSO が下り、3 パケットが 1 つずつ届く。sendmmsg の経路と io_uring の送出の経路（使えるときだけ）の両方）。
- 単体: `gso_socket_keeps_single_large_packets_whole`（B-361）。
- E2E: HTTP/3 の E2E は既定の設定（GSO 有り）で回る。

## 計測

aofish（Docker Desktop・WSL2、16 論理 CPU）のコンテナ間で、h2load の HTTP/3（`--alpn-list=h3 -t4 -c100 -m10 -D 20`）、
54,576 バイトの静的配信（`/`）と中継（`:8443`）。同じ周回の中で交互に回した。

| 回 | 条件 | 静的 rps | 中継 rps | サーバの CPU / 要求 |
|---|---|---|---|---|
| lab12 | veil GSO 無し → 有り（B-361 あり） | 22.6k・21.3k → 36.1k・36.7k | | 340µs → 146µs |
| lab13 | 両方 GSO 有り（veil は B-361 あり） | veil 35.6k・33.1k・32.4k / nginx 44.4k・44.7k・41.2k | veil 25.5k・23.9k・24.1k / nginx 32.5k・31.4k・30.8k | |
| lab15 | 両方 GSO 有り（B-361 修正後） | veil 36.9k・36.3k・32.8k / nginx 43.4k・40.9k・38.4k | veil 27.7k・26.4k・27.8k / nginx 30.0k・27.1k・27.4k | veil 152〜191µs / nginx 192〜244µs |

- GSO で veil のサーバの CPU は 1 要求あたり半分以下になり、負荷側（h2load）も GRO で受け取れて 1 要求あたり約 120µs →
  61µs になった。
- B-361 の修正で、GSO 有りの veil のパケットが 1452 バイトに戻った（静的配信が同じ周回で 33.1k → 38.8k。lab14）。
- 両方 GSO 有りで、中継は nginx と同程度、静的配信は veil が 1〜2 割低い。サーバの CPU は veil の方が少なく、負荷側も
  veil に対しては飽和していない（1 スレッド 89% 対 nginx 99%）。1 接続の速さ（同時 10 本 ÷ 応答時間）が律速で、
  残りの差は別に調べる。
- 最終のビルド（F-311 まで）のフルスイート（lab51）と、syscall・応答時間・HTTP/3・接続の張り直しの計測（lab56・lab59・lab60・lab62）は
  [docs/perf/README.md](../../perf/README.md) の「feat/beat-nginx」に記録した。
