# B-504: HTTP/1.1 で要求ヘッダの段の後に veil が本文のエラー（上限の 413・壊れた chunked の 400）を返すと、`proxy_on_log` が呼ばれない

**状態: 未着手（2026-11-26、feat/proxy-wasm-perf の [F-329](../features/F-329-wasm-single-fiber-entry-per-request.md) の E2E で見つけた。
main〈3d8148a0〉からある。F-329 の前後で変わらない）**

## 事象

- 本文の段を受けるモジュール（`proxy_on_request_body` をエクスポートする）のルートで、HTTP/1.1 の要求の本文が最初の読み取りに
  揃っていないか上限を超えているとき（F-329 の `body_in_hand` が受けないとき）、veil はヘッダの段（`proxy_on_context_create`・
  `proxy_on_request_headers`）を呼んでから本文を読む。本文が上限（Content-Length は `max_request_body_size`、chunked は
  `max_chunked_body_size`）を超えると 413、chunked が壊れていると 400 を書いて閉じる（`src/proxy/h1.rs` の `read_full_request_body` の
  `Err`）。このとき `proxy_on_log` を呼ばない。context は要求の状態を手放すときに消す表へ積まれ、次にインスタンスを使うときに
  `proxy_on_delete` だけが呼ばれる（B-177 の切断と同じ扱い）。
- 自己完結型 E2E（`tests/standalone/wasm_chain_tests.rs` の最後の窓。上限を超える chunked）で、413 の要求について作成・ヘッダ +1、
  本文・ログ +0、削除 +1（次の要求で）。Content-Length が上限を超える要求も同じ経路を通る（`body_in_hand` は上限を超える
  Content-Length を受けず、ヘッダの段の後の `read_full_request_body` が 413 を返す。コードで確かめた）。
- HTTP/2・HTTP/3 は本文を溜めてからヘッダの段を呼ぶので、上限を超える要求はモジュールを呼ばずに 413 を返す（context を作らない。
  同じ E2E で確かめた）。

## 影響

- Envoy は、ローカル応答（バッファの上限の 413 等）で終わったストリームも含め、HTTP context を作った要求の全部で `onLog` を呼ぶ。
  `proxy_on_log` でアクセスログ・メトリクスを書くフィルタは、veil の HTTP/1.1 では本文のエラーの要求を数えない。
- 「ヘッダの段を呼んだ要求にはログの段も呼ぶ」が、HTTP/1.1 の本文のエラーの経路だけ成り立たない（モジュールから見ると、
  プロトコルによって同じ要求が見えたり見えなかったりする）。

## 直し方（案）

- `read_full_request_body` の `Err` の経路で、エラーの応答を書いた後に `on_request_complete_async(&modules_to_apply)` を呼ぶ（ログの段と
  「ログ済み」）。読み捨て（`lingering_drain_before_close`。B-102）との順を決める（ログの段はゲストの時間の上限つき）。
- ログの段から見える応答のステータスは今は全経路で 0（[F-336](../features/F-336-wasm-on-log-response-values.md)）。413・400 を
  見せるかは F-336 と一緒に決める。
- 試験: `wasm_chain_tests` の最後の窓の期待（ログ +0）を +1 に変え、Content-Length の 413 と壊れた chunked の 400 を足す。

## 関連

- [F-329](../features/F-329-wasm-single-fiber-entry-per-request.md)（E2E で見つけた）・[F-336](../features/F-336-wasm-on-log-response-values.md)・
  B-177（ログの段まで行かなかった要求の context）・B-102（未読の本文を残して閉じない）
