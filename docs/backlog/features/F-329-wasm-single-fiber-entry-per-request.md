# F-329: ローカル応答の要求を 1 回のファイバーの出入りで終える（GET 2 → 1 回、POST 3 → 1 回）

**状態: 未着手（[F-327](F-327-proxy-wasm-path-cost.md) の子。`proxy_on_log` の時機が変わるので、入れる前に利用者の確認を得る）**

## 現状

- `instance_reuse` のプールのインスタンスでは、1 要求でゲストへ入るのは `call_async`（ファイバーの出入り）が GET で 2 回
  （`[ContextCreate, RequestHeaders]`・応答を書いた後の `[Log, Delete]`。`exports::call_chain`、F-319）、POST で 3 回
  （間に `[RequestBody]`）。
- 1 回の出入りごとに: wasmtime のファイバーの作成と破棄（`make_fiber`・`Box::new` のクロージャ・`wasmtime_fiber_init`・
  全ワーカー共有のエンジンの `Arc` の複製と破棄・`StoreFiber` の破棄）、veil の段取り（`Box::pin` の future、モジュールの名前の検索、
  `sessions.find`・`slot.acquire`、`set_request_attrs`、`arm_store`、`HttpContext::recycle`、`release`）。
  プロファイル（h1 GET）で wasmtime の出入りは 0.80 µs/要求（2 回分）、後始末の段の veil の段取りは約 0.3 µs。
- 後始末の段（`on_request_complete_async`）は応答をソケットへ書いた後に走る。

## 改修内容

1. ローカル応答の要求: 連続呼び出しの中で `proxy_send_local_response` が呼ばれ、保留中の外部呼び出し（`dispatch_http_call`・gRPC）が無ければ、
   同じ連続呼び出しの中で続けて `proxy_on_log`・`proxy_on_delete` を呼ぶ。応答はその後で書く。2 回目の出入りと段取りが丸ごと消える。
   `proxy_on_log` の前に `http_ctx.response_status` 等をローカル応答の値にする（今は 0）。
2. POST: 本文の全量がヘッダの段の時点で手元にあれば（HTTP/1.1 は読み取りバッファに Content-Length 分・chunked は終端まで、
   HTTP/2 は F-298 の END_STREAM 付き DATA、HTTP/3 は同じ束の本文）、`proxy_on_request_headers` が `Continue` を返したら同じ連続呼び出しで
   `proxy_on_request_body` を呼び、ローカル応答なら 1 のとおり後始末まで続ける。全量が無いときは今のまま。
3. 複数のモジュールのルート: Envoy と同じく全モジュールのヘッダの段の後に本文の段を呼ぶ順を守る（2 は単一モジュールに限る）。ローカル応答を
   返したモジュールより後のモジュール（ヘッダの段を呼んでいないもの）の扱いは今の挙動（作成・ログ・削除）を保つ。

## 意味の変化と危険

- `proxy_on_log` が、応答をソケットへ書き終える前（応答が決まった直後）に走る。ログの段から見える応答のステータス・ヘッダ・本文の長さは
  決まっているが、書き込みの失敗と書き込みにかかった時間は見えない。Envoy もローカル応答では応答を接続のバッファへ符号化した後に
  アクセスログを呼び、ソケットへの送出を待たないと考えている（未確認。着手の前に Envoy のソースで確かめる）。ガイド（`docs/guide/` の
  Proxy-Wasm の章、英日）に書く。
- 今はローカル応答の要求のログの段の `response.code` が 0（`set_response` は応答ヘッダの段でしか呼ばれない。`engine.rs:1234`）。
  1 でローカル応答の値を入れるので、ここは直る。
- `proxy_on_log`・`proxy_on_delete` がトラップしても、決まったローカル応答は返し（B-299）、インスタンスは捨てる。
- 段ごとの CPU の上限（`max_execution_time_ms`、`Window::Callback`）と協調的な譲り（エポック）は今の連続呼び出しと同じに保つ。

## 受け入れ条件

- aofish の交互 A/B で、h1 GET・h2・h3・json-tls の 1 要求のユーザー時間が 0.4 µs 以上、POST が 1.0 µs 以上下がる。
- E2E: 各 context の作成・ログ・削除が 1 回ずつ（GET・POST・chunked・HTTP/2・HTTP/3）、`proxy_on_log` から見える応答のステータス、
  `proxy_on_log` のトラップでも応答が届くこと、長時間の負荷でゲストの線形メモリが増え続けないこと（SDK の文脈の表）。
- 複数のモジュールのルートの段の順の試験（既存の E2E が通る）。

## 関連

- [F-327](F-327-proxy-wasm-path-cost.md)（親）・[F-319](F-319-wasm-per-request-fixed-cost.md)（連続呼び出し・`phases` を見送った理由）・F-298・B-299
- `docs/artifacts/proxy-wasm-perf/gemini/ans-01-host-path.md` の候補 1・5(d)
