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

## 設計

1. **連続呼び出しに条件つきの続きを足す**（`src/wasm/exports.rs`）
   - `Step::RequestBody(context_id, body_size, end_of_stream)` を足し、`MAX_STEPS` を 5 にする（作成・ヘッダ・本文・ログ・削除）。
   - `run_chain` は本文の段を、前のヘッダの段の戻り値が Continue（0）か本文を待つ Pause（1。`allow_request_body_read` のとき）で、
     ローカル応答が無く、保留の外部呼び出し（`pending_http_calls`・gRPC）が無いときだけ呼ぶ。満たさなければそこで止め、
     今の非同期の経路（`settle` の外部呼び出しのインラインの解決・本文の段の別呼び出し）に任せる。
   - 終わりの段（呼び出し元が許したときだけ）: 段の後にローカル応答が決まっていて保留の外部呼び出しが無ければ、ログの段の
     `response.code` をローカル応答の値にし、`proxy_on_log`・（燃料と締切を入れ直して）`proxy_on_delete` を続けて呼び、
     `Chain` に「終えた」と返す。
2. **エンジン**（`src/wasm/engine.rs`）: 効く並び（`resolve_effective_modules`）がモジュール 1 つで、`instance_reuse` で、要求の並び
   （`RequestSessions`）があるときだけ終わりの段を許す。終えたら `release_session(.., end = true)` で context を消し、`RequestSessions` に
   「ログ済み」（`Cell<bool>`）を立てる。`on_log_with_modules` は「ログ済み」なら何もしない（全プロトコルの後始末の呼び出し元
   〈`on_request_complete_async`〉は変えない）。
3. **本文の全量が手元にある POST**: 新しい入口（`on_request_with_body_async`、仮）が作成・ヘッダ（`end_of_stream = false`）・本文
   （`end_of_stream = true`）・ログ・削除を 1 回で呼び、`FilterResult::Continue { headers, body: Some(..) }`（今の型の `body`）か
   ローカル応答を返す。プロトコルの側は次のときだけ使い、後の本文の段を飛ばす:
   - 全量がある: HTTP/1.1 は Content-Length 分が最初の読み取りにある、chunked は終端が見えていて読み取り無しで復号できる。
     HTTP/2 は F-298 の溜める経路、HTTP/3 は `take_first_body` で全量。
   - 本文の上限（`limit`）以下で、本文の段が要る（`wants_request_body`）。上限超過の 413・`Expect: 100-continue`・分割して届く本文は今の経路。
4. **複数のモジュールのルート**: 変えない（モジュールをまたぐ `proxy_on_log` の順〈Envoy はフィルタの順〉を保つため）。
5. ゲストから見える呼び出しの順と引数は今と同じ（作成 → ヘッダ〈`end_of_stream = false`〉→ 本文〈全量、`end_of_stream = true`〉→ ログ → 削除）。
   変わるのは `proxy_on_log` の時機だけ（下）。

## 意味の変化と危険

- `proxy_on_log` が、応答をソケットへ書き終える前（応答が決まった直後）に走る。ログの段から見える応答のステータス・ヘッダ・本文の長さは
  決まっているが、書き込みの失敗と書き込みにかかった時間は見えない。Envoy もローカル応答では応答を接続のバッファへ符号化した後に
  アクセスログを呼び、ソケットへの送出を待たないと考えている（未確認。着手の前に Envoy のソースで確かめる）。ガイド（`docs/guide/` の
  Proxy-Wasm の章、英日）に書く。
- 今はローカル応答の要求のログの段の `response.code` が 0（`set_response` は応答ヘッダの段でしか呼ばれない。`engine.rs:1234`）。
  1 でローカル応答の値を入れるので、ここは直る。
- `proxy_on_log`・`proxy_on_delete` がトラップしても、決まったローカル応答は返し（B-299）、インスタンスは捨てる。
- 段ごとの CPU の上限（`max_execution_time_ms`、`Window::Callback`）と協調的な譲り（エポック）は今の連続呼び出しと同じに保つ。
- 今は応答の書き込みに失敗すると HTTP/1.1 は `proxy_on_log` を呼ばずに戻る（context は後の要求の `take_pending_deletes` で消す）。
  1 の後は書き込みの成否に関わらず `proxy_on_log` が走る（Envoy はストリームの破棄でアクセスログを呼ぶ）。
- 2 の判定（全量がある・上限以下）を誤ると、今は 413 を返す要求の本文をゲストへ渡す。判定は今の本文の読み取り
  （`read_full_request_body`）と同じ関数で行う。

## 受け入れ条件

- aofish の交互 A/B で、h1 GET・h2・h3・json-tls の 1 要求のユーザー時間が 0.4 µs 以上、POST が 1.0 µs 以上下がる。
- E2E: 各 context の作成・ログ・削除が 1 回ずつ（GET・POST・chunked・HTTP/2・HTTP/3）、`proxy_on_log` から見える応答のステータス、
  `proxy_on_log` のトラップでも応答が届くこと、長時間の負荷でゲストの線形メモリが増え続けないこと（SDK の文脈の表）。
- 複数のモジュールのルートの段の順の試験（既存の E2E が通る）。

## 関連

- [F-327](F-327-proxy-wasm-path-cost.md)（親）・[F-319](F-319-wasm-per-request-fixed-cost.md)（連続呼び出し・`phases` を見送った理由）・F-298・B-299
- `docs/artifacts/proxy-wasm-perf/gemini/ans-01-host-path.md` の候補 1・5(d)
