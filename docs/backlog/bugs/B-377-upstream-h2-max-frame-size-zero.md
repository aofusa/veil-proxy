# B-377: 上流の HTTP/2 が SETTINGS_MAX_FRAME_SIZE に 0 を送ると、ワーカーがメモリの尽きるまで止まらなかった

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx の 2 回目のセキュリティスキャン（`CLAUDE-SECURITY-20261114-024333`）の候補から確認。

## 事象

- 上流の HTTP/2 の多重化接続（`http2::upstream_mux`。h2c・ALPN で h2 を選んだ HTTPS・gRPC）は、上流の SETTINGS の
  `SETTINGS_MAX_FRAME_SIZE` を範囲を見ずに `FrameEncoder::set_max_frame_size` と `peer_max_frame` へ入れていた。
- 0 を受けると、要求の HEADERS を HEADERS＋CONTINUATION に分ける繰り返しが 1 バイトも進まず、送信キューへ
  空のフレームを積み続ける（ワーカーのスレッドが止まり、メモリが尽きる）。DATA も 0 バイトずつになる。
  上流（またはその経路にいる者）1 つで、その上流を使う全部の要求を受けるワーカーを止められる。
- 下流（クライアント側）の HTTP/2（`http2::connection`・`settings`）は RFC 9113 §6.5.2 の範囲（16384〜16777215）を
  検査済みで、上流の経路だけ抜けていた。

## 対応

- 範囲外の値は接続エラーにする（`ConnError`。接続を閉じ、進行中のストリームは再送しない異常終了）。

## テスト

- `http2::upstream_mux::tests::upstream_rejects_out_of_range_max_frame_size`（0・16383・16777216）。
