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

0. **先に直すもの**: `wants_request_body`・`wants_response_body` の `registry.get_module`（全ワーカー共有の `Arc<LoadedModule>` の複製と
   破棄）を `loaded_module`（借りるだけ）にする（[F-331](F-331-wasm-per-request-bookkeeping.md) の 1 項目。POST の計測と 3 の判定に乗るので先に）。
1. **連続呼び出しに条件つきの続きを足す**（`src/wasm/exports.rs`）
   - `Step::RequestBody(context_id, body_size, end_of_stream)` を足し、`MAX_STEPS` を 5 にする（作成・ヘッダ・本文・ログ・削除）。
   - 段ごとの CPU の上限: 本文・ログ・削除の段の前に `metering::arm` で燃料と締切を入れ直す（今は別々の呼び出しで段ごとに入れ直している。
     作成とヘッダの組だけ今と同じく 1 つの窓）。燃料の消費のメトリクスは段ごとに観測する（今と同じ段の名前で）。
   - 本文は連続呼び出しの前に置かない（ヘッダの段から本文と `request.total_size` が見えてしまう）。`GuestExports` に本文（`Bytes`）を預け、
     `run_chain` が本文の段の直前に `http_ctx.set_request_body(body, true)` する。
   - `run_chain` は本文の段を、ヘッダの段の戻り値が Continue（0）か本文を待つ Pause（1）で、`allow_request_body_read` があり、ローカル応答が無く、
     保留の外部呼び出し（`pending_http_calls`・gRPC）が無いときだけ呼ぶ。満たさなければそこで止め、今の非同期の経路（`settle` の外部呼び出しの
     インラインの解決・本文の段の別呼び出し）に任せる。
   - 終わりの段（呼び出し元が許したときだけ）: 段の後にローカル応答が決まっていて保留の外部呼び出しが無ければ、`http_ctx.response_status` を
     ローカル応答の値にし、応答ヘッダはローカル応答のヘッダを `std::mem::swap` で `http_ctx.response_headers` へ貸して（写さない）、
     `proxy_on_log` を呼んでから戻す（Envoy の `onLog` はローカル応答のステータスとヘッダを見せる。ans-04 の 1）。続けて `proxy_on_delete`。
     `next_context_id` が上限（`i32::MAX - 1`）に達したインスタンスは `log_pooled` と同じく削除の段を呼ばずに捨てる。
   - `Chain` の結果に、止まった段の種類（作成・ヘッダ・本文・ログ・削除）、ログの段に入ったか、本文の段の戻り値を持たせる。
2. **エンジン**（`src/wasm/engine.rs`）: 終わりの段の判断は `on_request_headers_with_modules` の本体に置く（HTTP/3 は `_async` を通らずに
   これを直接呼ぶ。`src/http3_server/request_task.rs`）。効く並び（`resolve_effective_modules`）がモジュール 1 つで、`instance_reuse` で、
   要求の並び（`RequestSessions`）があるときだけ許す（`src/mesh/wasm.rs` は `WasmModules::shared` で要求の並びが無く、`on_log` を呼ばない。
   これを壊さないための必須の条件）。結果の扱い:

   | 起きたこと | 扱い |
   |---|---|
   | ヘッダ・本文の段の失敗 | 今と同じ（段の名前で `on_failure`。`failure_policy`） |
   | 本文の段（`end_of_stream = true`）が保留の無い Pause | 今と同じく `record_failure("request_body", "unsupported_pause")`（B-300） |
   | ローカル応答を決めた後にその段でトラップ | `settle` と同じくローカル応答を返し（B-299）、インスタンスを捨てる。ログの段は走っていないので「ログ済み」は立てない（後始末は今の経路） |
   | ローカル応答の後のログ・削除の段の失敗 | `on_failure` を呼ばずにローカル応答を返し、インスタンスを捨てる（今の `log_pooled` と同じ）。ログの段に入ったので「ログ済み」を立てる |
   | 終えた | `release_session(.., end = true)` で context を消し、「ログ済み」（`RequestSessions` の `Cell<bool>`）を立てる |
   | `failure_policy` の 503 | 「ログ済み」は立てない（ログの段は走っていない） |

   `on_log_with_modules` は「ログ済み」なら何もしない（全プロトコルの後始末の呼び出し元〈`on_request_complete_async`〉は変えない）。
3. **本文の全量が手元にある POST**: 新しい入口（`on_request_with_body`、仮。HTTP/3 からも呼べるよう `_async` でない本体に置く）が作成・ヘッダ
   （`end_of_stream = false`）・本文（`end_of_stream = true`）・ログ・削除を 1 回で呼び、`FilterResult::Continue { headers, body: Some(..) }`
   （今の型の `body`）かローカル応答を返す。Continue のときは context を残す（後の応答の段・ログの段は今の経路）。プロトコルの側は次を
   全部満たすときだけ使う:
   - 本文の段が要る（`wants_request_body`）。WebSocket でない。`Expect: 100-continue` が無い（B-189）。トレーラが無い（HTTP/2・HTTP/3 の
     トレーラ付きの POST は本文の段の後にトレーラの段がある）。
   - 全量がある: HTTP/1.1 の Content-Length は最初の読み取りに全部ある（上限 `limit` 以下）。chunked は上限つきの復号器
     （`ChunkedDecoder::new_request(limit)`。今の `chunked_end` の判定は上限 0〈無制限〉）で終端まで復号できる。上限超過・不正なら 413 を
     先に返さず今の 2 段の経路へ落とす（ヘッダの段で 401・403 を返すフィルタの挙動を変えない）。空の chunked（`0\r\n\r\n`）も
     本文の段（長さ 0）を呼ぶ。HTTP/2 は F-298 の溜める経路、HTTP/3 は `take_first_body` で全量。
   - Continue のとき、HTTP/1.1 は今の本文の段の後と同じく `set_body_framing_headers` で上流へのフレーミングを書き換え（chunked を外して
     Content-Length）、後の本文の段を明示の印で飛ばす（書き換えを忘れると上流へ chunked のヘッダのまま復号した本文を送る）。
4. **複数のモジュールのルート**: 変えない（モジュールをまたぐ `proxy_on_log` の順〈Envoy はフィルタの順〉を保つため）。
5. ゲストから見える呼び出しの順と引数は今と同じ（作成 → ヘッダ〈`end_of_stream = false`〉→ 本文〈全量、`end_of_stream = true`〉→ ログ → 削除）。
   変わるのは `proxy_on_log` の時機と、ログの段から見える応答（下）。

## 意味の変化と危険

- `proxy_on_log` が、応答をソケットへ書き終える前（応答が決まった直後）に走る。ログの段から見える応答のステータス・ヘッダは決まっているが、
  書き込みの失敗と書き込みにかかった時間は見えない。Envoy もローカル応答では、応答を接続のバッファへ符号化した後、同じイベントループの周回の
  ストリームの終わり（`ConnectionManagerImpl::doEndStream` → `filter_manager_.log(DownstreamEnd)`）でアクセスログ（`proxy_on_log`）を呼び、
  その後の遅延破棄で `proxy_on_delete` を呼ぶ。ソケットへの送出は待たない（ans-04 の 1。Envoy の `source/common/http/conn_manager_impl.cc`・
  `filter_manager.cc`・`source/extensions/common/wasm/context.cc`）。違いは veil が符号化の前に呼ぶこと。ガイド（`docs/guide/` の Proxy-Wasm の章、
  英日）に書く。
- 今はローカル応答の要求のログの段の `response.code` が 0 で応答ヘッダも空（`set_response` は応答ヘッダの段でしか呼ばれない。`engine.rs:1234`）。
  1 で直る。ただし外部呼び出し（`dispatch_http_call`）の応答で決まったローカル応答は今の経路のままなので 0 のまま（範囲外。ans-04 の 6-6）。
- 今は応答の書き込みに失敗すると HTTP/1.1 は `proxy_on_log` を呼ばずに戻る（context は後の要求の `take_pending_deletes` で消す）。
  1 の後は書き込みの成否に関わらず `proxy_on_log` が走る（Envoy はストリームの破棄でアクセスログを呼ぶ）。
- 段ごとの CPU の上限（`max_execution_time_ms`、`Window::Callback`）と協調的な譲り（エポック）は今と同じに保つ（1 の入れ直し）。
- 3 の判定を誤ると、今は 413 を返す要求の本文をゲストへ渡す・上流へ壊れたフレーミングを送る。判定と書き換えは今の本文の読み取り
  （`read_full_request_body`）・本文の段の後の処理と同じ関数で行う。
- [F-331](F-331-wasm-per-request-bookkeeping.md) で `RequestSessions` を使い回すなら「ログ済み」を戻す（戻し忘れると以後の要求の
  `proxy_on_log`・`proxy_on_delete` が飛ばされて context が漏れる）。
- `cfg(feature)`: `wasm` 無し（`--no-default-features`）・`http2`・`http3` 無しでコンパイルできること。
- 影響を確かめる呼び出し元: `src/proxy/h1.rs`・`h2.rs`（トレーラ付きの POST）・`ws_tunnel.rs`、`src/http3_server/request_task.rs`、
  `src/mesh/wasm.rs`、`src/wasm/tests.rs` の既存の単体試験（`sessions.len()` の確認等）。

## 受け入れ条件

- aofish の交互 A/B で、h1 GET・h2・json-tls の 1 要求のユーザー時間が 0.4 µs 以上、h3 が 0.25 µs 以上（h3 の出入りは 2 回で 0.40 µs）、
  POST（Content-Length・chunked）が 1.0 µs 以上下がる。
- E2E: 各 context の作成・ログ・削除が 1 回ずつ（GET・POST・chunked・空の chunked・HTTP/2・HTTP/3・トレーラ付きの POST）、`proxy_on_log` から
  見える応答のステータスとヘッダ、ログ・削除の段のトラップでも応答が届き「ログ済み」で二重に呼ばれないこと、本文の段の Pause の 503（B-300）、
  上限を超える chunked の 413、`Expect: 100-continue`、ヘッダの段の間に本文が見えないこと、`max_execution_time_ms` が段ごとに効くこと、
  長時間の負荷でゲストの線形メモリが増え続けないこと（SDK の文脈の表）。
- 複数のモジュールのルートの段の順の試験（既存の E2E が通る）。io_uring と epoll の両方。

## 関連

- [F-327](F-327-proxy-wasm-path-cost.md)（親）・[F-319](F-319-wasm-per-request-fixed-cost.md)（連続呼び出し・`phases` を見送った理由）・F-298・B-299
- `docs/artifacts/proxy-wasm-perf/gemini/ans-01-host-path.md` の候補 1・5(d)、`ans-04-design-review.md` の 1・6
