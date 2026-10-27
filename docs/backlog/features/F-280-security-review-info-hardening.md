# F-280: 2026-10 のセキュリティレビューの Info 項目の小さな堅牢化（CQ overflow の監視・accept の fd・0-RTT の不変条件の試験・レートリミット表の衝突の文書化）

**状態: 完了（v0.10.0）**

## 今の状態

欠陥ではないが、設計の前提をコードと試験で固定しておきたい項目。

- RT-07: `runtime/uring/ring.rs` は CQ overflow（`IORING_SQ_CQ_OVERFLOW`）と `sq_off.dropped` を監視していない。`IORING_FEAT_NODROP` と park 時の
  `GETEVENTS` で実害は無いが、multishot の受信で CQ が溢れると multishot が終端して再アームに頼る。
- RT-08: `runtime/uring/tcp.rs::Accept::poll` は `storage_to_sockaddr` が `Err` のとき accept 済みの fd を閉じずに返す。`AF_INET`/`AF_INET6` 以外は
  来ないので到達しないが、`?` の前に fd を閉じる形にしておくと安全。
- PR-07: 0-RTT（`enable_early_data()`）を受け付けるが、h3 の接続の生成は `conn.is_established()` の後に限られる（`event_loop.rs:395`・`mod.rs:1054`・
  `upstream.rs:775`）ので、ハンドシェイクの完了前に要求を処理しない（再送攻撃で副作用が起きない）。この不変条件に依存しているのに
  コメントと試験が無い。
- PR-06: `config/runtime_state.rs::RateTable` は `(ルート, IP)` を鍵付きハッシュで 65536 スロットへ畳む固定表。メモリは有界で狙い撃ちは困難だが、
  多数の送信元からの衝突で無関係なクライアントが 429 を受け得る（既知のトレードオフ。文書に無い）。

## 改修内容

- RT-07: `consume_cqes` の後に `IORING_SQ_CQ_OVERFLOW` を見て `debug!` + カウンタ（`veil_uring_cq_overflow_total`）。multishot の終端の再アームが
  既にあることを試験で固定する。
- RT-08: `Accept::poll` で変換の失敗時に `close(fd)` してからエラーを返す。
- PR-07: `is_established()` のゲートに不変条件のコメントを書き、0-RTT のデータを含む Initial で要求が処理されないことを E2E で固定する
  （quinn の `0rtt` で送る）。
- PR-06: `docs/guide/`（英・日）のレートリミットの節に衝突の性質と `RateTable` の大きさを書く。

## 修正（v0.10.0）

RT-07: CQ の溢れを SQ の flags で検出して GETEVENTS で移し veil_uring_cq_overflow_total で数える（単体で溢れ→flush で 16 件全部受ける）。RT-08: accept の変換失敗で fd を閉じる。PR-07: init_h3 の不変条件のコメントと 0-RTT の要求がハンドシェイク完了まで処理されない単体試験（quiche の 0-RTT で確認。E2E の quinn ではなく単体で固定）。PR-06: routing.md（英日）にレート制限の表の衝突を記載。security.md（英日）に早期データの節、protocols.md の 0-RTT の記述と既定の cc（cubic）を訂正

## 関連

- [B-222](../bugs/B-222-http3-no-connection-limit-no-retry.md)、[B-31](../bugs/B-31-rate-limit-thread-local-per-worker.md)
