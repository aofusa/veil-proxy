# F-113: HTTP/3 データグラム recvmmsg/sendmmsg バッチングによる per-packet syscall 削減

## 機能説明

HTTP/3（quiche / userspace QUIC）の UDP データプレーンで、**1 データグラム = 1 syscall**
（`recvmsg` / `sendto` / `sendmsg`）となっている点を、`recvmmsg(2)` / `sendmmsg(2)` による
**複数データグラムの 1 syscall バッチ処理**へ置き換え、Docker veth（カーネル GSO/GRO オフロード
非対応）環境での HTTP/3 スループットを HTTP/1.1・HTTP/2 に近づける。

## 現状・背景

[F-111](F-111-perf-regression-check-http3-gso-zerocopy.md) で、HTTP/3 は HTTP/2 比
約 1/6.3（424.7 vs 2696 req/s）であり、主因は **userspace QUIC の per-packet syscall コスト**
という構造的制約だと結論づけた。同チケットは GSO 送信のゼロコピー化までは実施したが、
GSO/GRO はカーネル側で **同一フロー（5-tuple）** のセグメントしか結合できず、Docker veth では
オフロードが無効化される（[[project_perf_http3_grpc_ws]]）。

一方 `recvmmsg` / `sendmmsg` は **異なるフロー（異なる接続）** のデータグラムも 1 回の
syscall にまとめられる。多数の同時接続（h2load `-c100`）を捌くサーバでは、GSO/GRO が効かない
環境でも syscall 回数を実効的に削減できる。これは F-111 が **未着手** の経路であり、
「環境側の前提変更が必要」とした結論の一部を、アプリ側の I/O バッチングで前進させ得る。

## 現状のコード

- `src/udp/socket.rs`: 受信は `recv_with_gro_sync`（単一 `recvmsg` + GRO CMSG）、送信は
  `send_to_slice_async`（単一 `sendto`）/ `send_gso_combined_async`（単一 `sendmsg` + GSO）。
  いずれも **非ブロッキングソケット + io_uring POLL_ADD で readiness 待機** する epoll 型モデルで、
  1 呼び出し = 1 データグラム。
- `src/http3_server.rs`（メインループ）: ワーカーごとに 1 QUIC ソケットを保持し、
  受信→`quiche::recv`→`h3.poll`→`quiche::send`→送信 を繰り返す。

## 実測知見（F-112 計測 2026-07-12・`docs/perf/perf_f112_coverage_regression.md`）

- HTTP/3 は **429.7 req/s**（HTTP/2 比 16%）で **CPU 168%（4 コア中 42%）**。CPU バウンドではなく
  **syscall 往復律速**。→ CPU 余地があり、syscall 削減で上限を引き上げられる可能性が高い。
- **並行度・クライアントスレッドを変えてもスループットは横ばい**（h2load `-c25〜-c200`・`-t1/-t2`
  で ~340〜465 req/s）。ワーカーあたりの固定処理レートに律速され、負荷を増やすと
  レイテンシだけが伸びる（サーバ側直列化のシグネチャ）。
- 主ループ `select_biased!` は毎イテレーション負け arm（`notify.wait` と `sleep`）を生成し、
  `sleep(timeout)` は **io_uring タイマー SQE を arm→cancel** する。1 データグラムごとに
  この往復を払うため、per-datagram オーバーヘッドが実効上限を下げている。

### 第1段（低リスク・実装済み）: 受信 drain バッチ

`recvmmsg` の mmsghdr 導入前に、より低リスクな **受信 drain** を先行実装した:
1 回の readiness あたり、最初の 1 通は `select_biased!` 経由、以降は既存の
`recv_with_gro_sync`（非ブロッキング recvmsg）で EAGAIN まで（上限 `H3_RECV_DRAIN_MAX=64`）
掻き出し、`select`/タイマーの往復を drain バッチ全体で 1 回に償却する。追加確保なし・既存の
per-segment ゼロコピー処理を流用（`src/http3_server.rs` 主ループ）。真の `recvmmsg`/`sendmmsg`
（1 syscall で複数データグラム）は第2段として本チケットに残す。

## 改修案

1. **受信バッチ**: `recvmmsg` で最大 N（例: 32）データグラムを 1 syscall で取得し、
   各データグラムを送信元アドレスで対応する QUIC 接続へ振り分けて `conn.recv()` する。
   スレッドローカルの mmsghdr/iovec/バッファ配列を再利用し per-syscall のアロケーションを排除
   （ホットパス絶対規則: 追加確保・コピー禁止）。
2. **送信バッチ**: 1 イテレーションで全接続から取り出した送信パケットを `sendmmsg` で
   1 syscall にまとめて送出（宛先が異なるため GSO とは別軸。GSO と併用可: 各 mmsghdr が
   UDP_SEGMENT CMSG を持てる）。
3. **フォールバック**: `recvmmsg`/`sendmmsg` が EAGAIN/部分成功のときの継続と、単発経路への
   フォールバックを実装。GRO セグメントサイズ CMSG のパースは各 msg で維持。

## 受け入れ条件

- HTTP/3 の req/s が現状（静穏ホスト ~425 req/s）から有意に改善（目標: HTTP/2 の 50%+ を志向。
  ただし環境依存のため達成保証はしない）。
- **HTTP/1.1・HTTP/2・gRPC に回帰がない**ことを A/B 実測で確認（F-111 と同じ計測手順）。
- HTTP/3 E2E（`test_http3_*` / gRPC over H3）が全てグリーン。
- 新規 io_uring オペコードを増やさない（POLL_ADD readiness + 同期 mmsg のモデルを維持、
  もしくは io_uring の SENDMSG/RECVMSG マルチショットを別途評価）。

## 依存・リスク

- QUIC 接続状態はワーカー内で完結（reuseport 分散）のため、`recvmmsg` で受けた各データグラムの
  送信元がワーカー内接続テーブルに存在する前提。新規接続（Initial）混在時の振り分けに注意。
- Docker veth では GSO/GRO 無効・カーネルオフロード非対応という前提は変わらない。改善幅は
  syscall 削減分に限られ、HTTP/2 完全同等は保証できない（F-111 の構造的結論は残り得る）。
- 変更は `src/udp/socket.rs` と `src/http3_server.rs` のホットパスに及ぶため、
  キャンセル安全性（F-87）・SQ 満杯（B-24）・Drop UAF（B-07）の不変条件を維持する。
- 関連: [F-33](F-33-http3-gso-gro-offload.md) / [F-45](F-45-http3-gro-batch-recv.md) /
  [F-60](F-60-http3-gro-batch-autosize.md) / [F-111](F-111-perf-regression-check-http3-gso-zerocopy.md)。
</content>
