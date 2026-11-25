# B-499: HTTP/1.1 で Wasm のローカルレスポンスの後に閉じるとき、応答に `Connection: close` を付けていない（クライアントは閉じられる接続で次の要求を送って失う）

**状態: 完了（2026-11-25、feat/httparena-perf。F-317 の aofish の計測で見つけた）**

## 事象

- aofish（io_uring のビルド、HttpArena のエントリと同じ設定）で、Proxy-Wasm のフィルタが答えるルート（`/baseline11`・`/pipeline`）へ
  veil-load の HTTP/1.1（512 接続、パイプライン 16 本では 256 接続）を当てると、5 秒の計測ごとに約 125 本の接続が応答の後に
  黙って閉じられ、続けて送った要求が失われた（パイプラインでは 1 回の計測で約 2,000 要求）。wrk の json-tls（同じ Wasm のフィルタが
  答える `/json`）でも、計測によって 256 本の接続の全部が読み取りのエラーになった。`Respond` のルート（Wasm 無し）と epoll のビルドでは出ない。
- 原因: 接続の集約（F-303。`[performance] concentrate_connections` の既定は Linux の TCP で有効）が既存の接続を動かすとき、
  HTTP/1.1 は `take_shed()` が真の要求で `client_wants_close` を立て、応答の後に閉じる。応答に `Connection: close` を付ける規則
  （B-360）は上流の応答・静的配信・固定応答の経路には入っていたが、**Wasm のローカルレスポンス**（`wasm_local_response_h1`。要求ヘッダ・
  要求本文・応答ヘッダ・応答本文の各段で `send_local_response` したとき）は付けていなかった。クライアントは続けられる接続だと思って
  次の要求を送り、サーバは閉じる（RFC 9112 §9.6: 閉じる側は最後の応答に `close` を付ける）。
- クライアントの `Connection: close`・WebSocket の要求・`Expect: 100-continue` を待っている chunked の要求も同じ（閉じるのに
  応答が `close` を言わない）。io_uring だけで出たのは、この計測では epoll のビルドで接続の集約が接続を動かさなかったため（veil-load の張り直しが 0）。

## 対応

- `wasm_local_response_h1(resp, head, close)` と `sendfile::write_h1_wasm_local(.., close)`: 応答の後に閉じるときは
  `Connection: close` を付け、モジュールが付けた `Connection`・`Keep-Alive` は出さない。
- 呼ぶ側（`proxy/h1.rs` の要求ヘッダ・要求本文の段、`sendfile.rs`・`forward.rs`・`buffered.rs`・`https.rs`・`compress.rs` の応答ヘッダ・
  応答本文の段）は、それぞれが持っている閉じるかどうか（`client_wants_close`、上流の経路は B-360 の `close`）を渡す。要求ヘッダの段は
  `client_wants_close || WebSocket || Expect を待っている chunked`（閉じると分かっているもの）。
- 試験: `proxy::h1::b188_wasm_local_response_tests::closing_response_says_connection_close`。aofish の計測（veil-load は
  `Connection: close` を見て張り直すようにした）で、io_uring の Wasm のルートの失った要求が 0 になること（F-317 の計測の記録）。

## 関連

- [F-317](../features/F-317-httparena-infra-score-plan.md)・F-303・B-360・F-271・B-300
