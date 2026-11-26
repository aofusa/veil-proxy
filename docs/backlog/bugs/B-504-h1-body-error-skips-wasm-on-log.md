# B-504: 要求ヘッダの段の後に veil が応答を返して終える一部の経路で `proxy_on_log` が呼ばれない（HTTP/1.1 の本文のエラー・書き込みの失敗、WebSocket の 502、HTTP/2 のメモリのファイルの応答ヘッダの段のローカル応答）

**状態: 完了（2026-11-26、feat/proxy-wasm-perf。[F-329](../features/F-329-wasm-single-fiber-entry-per-request.md) の E2E で見つけ、
Gemini のレビュー〈ans-11〉で経路を洗い直した。main〈3d8148a0〉からある。F-329 の前後で変わらない）**

## 事象

要求ヘッダの段（`proxy_on_context_create`・`proxy_on_request_headers`）を呼んだ要求を、veil が応答を書いて（または書こうと
して）終えるのに `proxy_on_log` を呼ばない経路があった。context は要求の状態を手放すときに消す表へ積まれ、次にインスタンスを
使うときに `proxy_on_delete` だけが呼ばれた（B-177 の切断と同じ扱い）。

- **HTTP/1.1 の本文の読み取りのエラー**（`src/proxy/h1.rs` の `read_full_request_body` の `Err`。本文の段を受けるモジュールの
  ルートで、本文がヘッダの段の後に届く・上限を超える要求）:
  - chunked の本文が `max_chunked_body_size` を超える → 413。
  - ヘッダより後の読み取りで届いた壊れた chunked → 400（ヘッダと同じ最初の読み取りにある壊れた chunked は、Wasm の前に 400
    で返す）。
  - 本文の読み取りのタイムアウト・途中の EOF（Content-Length・chunked とも）→ 400。
  - ストリーミングで受ける `Backend::Handler`（`handler` feature）に本文を読むモジュールを付け、Content-Length が上限を超える
    → 413。
  - 通常のルート（`Proxy`・`Respond`・`File`・`Redirect`・溜めるハンドラ）の Content-Length の上限超過は、`check_security`
    （`src/l7.rs`）と `MAX_BODY_SIZE` が Wasm の前に 413 を返す（モジュールを呼ばない。HTTP/2・HTTP/3 と同じ）。
- **HTTP/1.1 の WebSocket のアップグレードで健全な上流が無い** → 502（上流を選んだ後の接続の失敗・タイムアウトの 502・504 は
  `handle_websocket_proxy` が値を返し、ログの段を呼んでいた）。
- **HTTP/1.1 の応答の書き込みの失敗**: 要求の段のローカル応答（段ごとに呼ぶ経路）・chunked + `Expect: 100-continue` の
  `100 Continue`・パイプラインの溜めた応答の送出（WebSocket の前）・WebSocket の 101 とそれ以外の応答・`handle_backend` が
  `None` を返す全経路（下流への書き込みの失敗・タイムアウト）。HTTP/3 は失敗しても必ずログの段を呼び（`request_task.rs` の
  ラッパ）、F-329 の続けて呼ぶ経路のローカル応答は書く前にログの段を呼ぶので、HTTP/1.1 の段ごとの経路だけが違った。
- **HTTP/2・HTTP/3 の WebSocket（拡張 CONNECT。`src/proxy/ws_tunnel.rs`）で健全な上流が無い** → 502（HTTP/1.1 と同じ。トンネルの
  後と要求ヘッダの段のローカル応答はログの段を呼んでいた）。
- **HTTP/2 のメモリのファイル（`Backend::MemoryFile`）で応答ヘッダの段がローカル応答を返す**（`src/proxy/h2.rs` の
  `h2_dispatch`）: ローカル応答を送ってその場で関数から返し、関数の最後のログの段を飛ばしていた（`Proxy`・`SendFile` 等は値を
  返して最後を通る）。

HTTP/2・HTTP/3 は本文を溜めてからヘッダの段を呼ぶので、上限を超える本文はモジュールを呼ばずに 413 を返す（context を作らない）。

## 影響

- Envoy は、ローカル応答（バッファの上限の 413 等）で終わったストリームや下流がリセットしたストリームも含め、HTTP context を
  作った要求の全部で `onLog` を呼ぶ（Wasm のフィルタはストリームのアクセスログのハンドラとして登録され、ストリームの破棄で
  呼ばれる。`source/common/http/filter_manager.cc`・`source/extensions/common/wasm/context.cc`）。`proxy_on_log` でアクセスログ・
  メトリクスを書くフィルタは、veil ではこれらの要求を数えなかった。
- 「ヘッダの段を呼んだ要求にはログの段も呼ぶ」が経路とプロトコルで違った（同じ要求が見えたり見えなかったりする）。

## 直したこと

- HTTP/1.1（`src/proxy/h1.rs` の `handle_requests`）: ヘッダの段の後に要求を終える経路の全部で、閉じる前に
  `on_request_complete_async(&modules_to_apply)` を呼ぶ。本文の読み取りのエラーは、エラーの応答を書いた後・読み捨て
  （`lingering_drain_before_close`。B-102）の前に呼ぶ（読み捨ては相手が送り終えるまで待つので、ログの段を待たせない）。
  `on_request_complete_async` は要求の並びが「ログ済み」なら何もしない（F-329 の続けて呼ぶ経路で 2 回呼ばない）。
- HTTP/2（`h2_dispatch` の `Backend::MemoryFile`）: 応答ヘッダの段のローカル応答も値として返し、関数の最後のログの段を通す。
- HTTP/2・HTTP/3 の WebSocket（`ws_tunnel.rs`）: 健全な上流が無い 502 の後にもログの段を呼ぶ。
- 経路を足しただけで、エラーの無い要求の経路は変えない（ホットパスに分岐・確保を足さない）。
- ログの段は 1 要求に 1 回にした（`FilterEngine::on_log_with_modules` が要求の状態に「ログ済み」を置き、同じ要求でまた呼ばれたら
  何もしない。F-329 のローカル応答の印と同じもの）。呼ぶ箇所が増えたので、2 回呼ぶ経路を作っても context を作り直して
  `proxy_on_log` を 2 回呼ばない（Gemini のレビュー）。
- ガイド（英日の「VM の文脈の使い回し」）に、ログの段を呼ぶ範囲（veil 自身のエラー応答・書き込みの失敗を含む。ストリームの
  リセット等で要求を捨てたときだけ次の利用で消す）を書いた。

## 残すもの（範囲外）

- 要求の future ごと捨てたとき（HTTP/2・HTTP/3 のクライアントのストリームのリセット・接続の切断、停止の打ち切り）は、今までどおり
  ログの段を呼ばずに次の利用で `proxy_on_delete` だけを呼ぶ（B-177。捨てられた future の中からはゲストを呼べない）。
- ログの段から見える応答のステータスは全経路で 0（[F-336](../features/F-336-wasm-on-log-response-values.md)）。413・400 を
  見せるかは F-336 で決める。

## 試験

- 自己完結型 E2E `tests/standalone/wasm_chain_tests.rs`（io_uring・epoll）: 1 モジュール・`instance_reuse` のルートで、モジュールが
  数えた回数をプロトコルごとの差で見る。
  - HTTP/1.1 の上限を超える chunked（413）・`100 Continue` の後に届く壊れた chunked（400）: 作成・ヘッダ・ログ・削除が +1、本文 +0
    （以前はログ +0）。
  - 全プロトコルの `/mem`（メモリのファイル + 応答ヘッダの段のローカル応答）: ログ +1（以前は HTTP/2 で +0）。
  - 全プロトコルの Content-Length の上限超過（413）: モジュールを呼ばない（作成 +0）。
- 単体（`src/wasm/tests.rs` の `the_log_phase_runs_once_per_request`）: 同じ要求で `on_log` を 2 回呼んでも、作成・ログ・削除は
  1 回ずつ。
- WebSocket の健全な上流が無い 502（全プロトコル）・書き込みの失敗の経路は、同じ 1 行（`on_request_complete_async`）を足しただけで、
  自己完結型の再現が難しいので E2E は足していない（コードで確かめた）。

## 関連

- [F-329](../features/F-329-wasm-single-fiber-entry-per-request.md)（E2E で見つけた）・[F-336](../features/F-336-wasm-on-log-response-values.md)・
  B-177（ログの段まで行かなかった要求の context）・B-102（未読の本文を残して閉じない）・B-300（応答ヘッダの段のローカル応答）
