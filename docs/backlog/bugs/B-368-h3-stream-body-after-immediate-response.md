# B-368: HTTP/3 で即時に応答した要求の後ろの本文を `stream_bodies` に溜め続けていた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx のセキュリティスキャン（Claude Security、`CLAUDE-SECURITY-20261113-030705` の F1。HIGH）で判明。

## 事象

HTTP/3 の `process_h3_events` は、`Data` のイベントで読んだ本文を、そのストリームが要求を処理中かどうかを見ずに
`stream_bodies` へ足していた。分類の時点で応答を返し終えた要求（405・413・ルート無し・リダイレクト等の即時の応答。
`Decision::Handled`）は、その後もクライアントが本文を送り続けると、誰も読まない本文が接続の終わりまで溜まる。
1 接続で `initial_max_stream_data_bidi_remote` × 同時ストリーム数を超えて、ストリームを開き直すたびに増える
（フロー制御の窓は読んだ分だけ開くので、送り続けられる）。

## 対応

- `Data` のイベントは、バッファ経路の処理中の要求（`buffered_reqs`）か、見出しを読んだばかりの要求（`new_headers`）の
  ストリームだけ本文を読む。それ以外は `stream_shutdown(Read, H3_NO_ERROR)` で受信を止めて読み捨てる。
- `Decision::Handled` で本文が残る（`more_frames`）ストリームは受信を止める。`Finished` で処理中でないストリームの
  `stream_bodies` を消す。

## テスト

- `http3_server::tests::test_b368_body_after_immediate_response_is_not_buffered`（`:protocol` の無い CONNECT に 405 を
  返した後に本文を送っても `stream_bodies` が空のまま）。
