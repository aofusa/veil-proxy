# F-329: ローカル応答の要求を 1 回のファイバーの出入りで終える（GET 2 → 1 回、POST 3 → 1 回）

**状態: 未着手（[F-327](F-327-proxy-wasm-path-cost.md) の子。2 段階に分ける。段階 1〈ゲストから見える意味は変わらない〉は確認不要。
段階 2〈`proxy_on_log` の時機が変わる〉は入れる前に利用者の確認を得る）**

## 現状

- `instance_reuse` のプールのインスタンスでは、1 要求でゲストへ入るのは `call_async`（ファイバーの出入り）が GET で 2 回
  （`[ContextCreate, RequestHeaders]`・応答を書いた後の `[Log, Delete]`。`exports::call_chain`、F-319）、POST で 3 回
  （間に `[RequestBody]`）。
- 1 回の出入りごとに: wasmtime のファイバーの作成と破棄（`make_fiber`・`Box::new` のクロージャ・`wasmtime_fiber_init`・
  全ワーカー共有のエンジンの `Arc` の複製と破棄・`StoreFiber` の破棄）、veil の段取り（`Box::pin` の future、モジュールの名前の検索、
  `sessions.find`・`slot.acquire`、`set_request_attrs`、`arm_store`、`HttpContext::recycle`、`release`）。
  プロファイル（h1 GET）で wasmtime の出入りは 0.80 µs/要求（2 回分）、後始末の段の veil の段取りは約 0.3 µs。
- 後始末の段（`on_request_complete_async`）は応答をソケットへ書いた後に走る。

## 段階（2026-11-26。拡張性を性能より優先する方針〈[F-327](F-327-proxy-wasm-path-cost.md)〉に合わせて分ける）

- **段階 1（ゲストから見える意味は変わらない。確認は要らない）**: 本文の全量が手元にある POST で、作成・ヘッダ・本文の段を 1 回の出入りに
  まとめる（POST 3 → 2 回）。ログ・削除は今と同じく応答を書いた後に別の 1 回で呼ぶ。下の設計の 0、1 の本文の段まで、3、4、5。
  見込み POST −0.5〜0.7 µs/要求。
- **段階 2（`proxy_on_log` の時機が変わる。入れる前に利用者の確認を得る）**: ローカル応答の要求は、ログ・削除も最初の 1 回に入れる
  （GET 2 → 1 回、POST 2 → 1 回）。下の設計の 1 の終わりの段と 2。見込み GET・h2・json-tls −0.5〜0.7、POST さらに −0.5〜0.7 µs/要求。
  - HTTP/1.1・HTTP/2: 今は応答をソケットへ書いた後に呼ぶ。段階 2 の後は応答が決まった直後（書く前）。Envoy もソケットへの送出を待たずに
    呼ぶ（下の「意味の変化と危険」）ので、Envoy の挙動に近づく。
  - HTTP/3: 今も ACK を待たず、応答をメインループへ渡した直後に呼んでいる（`request_task.rs` の `handle_request` → `finish_h3_wasm_lifecycle`）。
    段階 2 で変わるのは渡す前か後かだけ。Envoy はログを後へ回す（`deferHeadersAndTrailers`。送った分の ACK を待つ仕組みと見ている。未確認）
    ので、どちらでも Envoy とは違う。HTTP/3 を対象にするかも確認する（外すと h3 は改善なし。試算で −8 点ほど）。
- **どちらの段階も今できることを減らさない**: ゲストから見える呼び出しの順・引数は変えない。今の `proxy_on_log` で読めるプロパティ
  （`src/wasm/host/properties.rs`: 要求の値・`response.code`・`response.code_details`・`request.time` 等）に、送出の結果に依存する値
  （送ったバイト数・送出にかかった時間等）は無い。`proxy_on_log` の中の外部呼び出しは今も解決しない（`log_pooled` はログと削除を
  続けて呼ぶ）ので変わらない。段階 2 で増えるのは、ログの段でローカル応答のステータス・ヘッダが見えること（今は 0 と空）と、書き込みに
  失敗した要求でも `proxy_on_log` が呼ばれること（今は呼ばれない）で、どちらも Envoy と同じ。

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
   - 終わりの段（段階 2。呼び出し元が許したときだけ）: 段の後にローカル応答が決まっていて保留の外部呼び出しが無ければ、`http_ctx.response_status` を
     ローカル応答の値にし、応答ヘッダはローカル応答のヘッダを `std::mem::swap` で `http_ctx.response_headers` へ貸して（写さない）、
     `proxy_on_log` を呼んでから戻す（Envoy の `onLog` はローカル応答のステータスとヘッダを見せる。ans-04 の 1）。続けて `proxy_on_delete`。
     `next_context_id` が上限（`i32::MAX - 1`）に達したインスタンスは `log_pooled` と同じく削除の段を呼ばずに捨てる。
   - `Chain` の結果に、止まった段の種類（作成・ヘッダ・本文・ログ・削除）、ログの段に入ったか、本文の段の戻り値を持たせる。
2. **エンジン**（`src/wasm/engine.rs`。段階 2）: 終わりの段の判断は `on_request_headers_with_modules` の本体に置く（HTTP/3 は `_async` を通らずに
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
   （`end_of_stream = false`）・本文（`end_of_stream = true`）を 1 回で呼び（段階 2 では、ローカル応答ならログ・削除まで続ける）、`FilterResult::Continue { headers, body: Some(..) }`
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

- （段階 2）`proxy_on_log` が、応答をソケットへ書き終える前（応答が決まった直後）に走る。ログの段から見える応答のステータス・ヘッダは決まっているが、
  書き込みの失敗と書き込みにかかった時間は見えない。Envoy の HTTP/1.1・HTTP/2 は、応答をコーデックへ符号化し終えたストリームの終わり
  （`ConnectionManagerImpl::doEndStream` → `doDeferredStreamDestroy`）で `stream.log(AccessLogType::DownstreamEnd)`（→ `proxy_on_log`）、
  `stream.filter_manager_.destroyFilters()`（→ `onDestroy` → `proxy_on_done`・`proxy_on_delete`）、`dispatcher_->deferredDelete(..)` の順に
  同期で呼び、ソケットへの送出を待たない（待つのはコーデックの完了〈`canDestroyStream`〉だけ。2026-11-26 に `source/common/http/conn_manager_impl.cc`
  を読んで確かめた。`onDestroy` から先は ans-04 の読解）。違いは veil が符号化の前に呼ぶこと。**HTTP/3 は違う**: Envoy は
  `doDeferredStreamDestroy` でログを呼ばず `deferHeadersAndTrailers()` で後へ回す（送った分の ACK を待つ仕組みと見ている。未確認）。veil の
  HTTP/3 は送出の前に呼ぶことになる。ガイド（`docs/guide/` の Proxy-Wasm の章、英日）に書く。
- 今はローカル応答の要求のログの段の `response.code` が 0 で応答ヘッダも空（`set_response` は応答ヘッダの段でしか呼ばれない。`engine.rs:1234`）。
  段階 2 でまとめた要求は直る。段階 2 を入れない場合や、まとめない要求（複数のモジュールのルート・外部呼び出しの応答で決まったローカル応答
  〈ans-04 の 6-6〉）は、Envoy に合わせる不具合の修正として別に直す（ローカル応答を決めたときにステータスとヘッダを要求の並びに持ち、
  今の経路のログの段の前に置く）。
- 今は応答の書き込みに失敗すると HTTP/1.1 は `proxy_on_log` を呼ばずに戻る（context は後の要求の `take_pending_deletes` で消す）。
  段階 2 の後は書き込みの成否に関わらず `proxy_on_log` が走る（Envoy はストリームの破棄でアクセスログを呼ぶ）。
- Envoy はローカル応答も応答の段（`proxy_on_response_headers`・`proxy_on_response_body`）を通すと見ている（`FilterManager::sendLocalReplyViaFilterChain`
  が先頭の encoder フィルタから符号化する。要確認）。veil は今通さない（互換性の差。範囲外で、確かめてから別に扱う）。合わせる場合に、
  連続呼び出しのログの段の前に応答の段を足せる形にしておく。
- 段ごとの CPU の上限（`max_execution_time_ms`、`Window::Callback`）と協調的な譲り（エポック）は今と同じに保つ（1 の入れ直し）。
- 3 の判定を誤ると、今は 413 を返す要求の本文をゲストへ渡す・上流へ壊れたフレーミングを送る。判定と書き換えは今の本文の読み取り
  （`read_full_request_body`）・本文の段の後の処理と同じ関数で行う。
- [F-331](F-331-wasm-per-request-bookkeeping.md) で `RequestSessions` を使い回すなら「ログ済み」を戻す（戻し忘れると以後の要求の
  `proxy_on_log`・`proxy_on_delete` が飛ばされて context が漏れる）。
- `cfg(feature)`: `wasm` 無し（`--no-default-features`）・`http2`・`http3` 無しでコンパイルできること。
- 影響を確かめる呼び出し元: `src/proxy/h1.rs`・`h2.rs`（トレーラ付きの POST）・`ws_tunnel.rs`、`src/http3_server/request_task.rs`、
  `src/mesh/wasm.rs`、`src/wasm/tests.rs` の既存の単体試験（`sessions.len()` の確認等）。

## 受け入れ条件

- aofish の交互 A/B で、段階 1 は POST（Content-Length・chunked）の 1 要求のユーザー時間が 0.4 µs 以上下がる。段階 2 は h1 GET・h2・json-tls が
  0.4 µs 以上、h3 が 0.25 µs 以上（対象にする場合。h3 の出入りは 2 回で 0.40 µs）、POST が段階 1 と合わせて 1.0 µs 以上下がる。
- E2E: 各 context の作成・ログ・削除が 1 回ずつ（GET・POST・chunked・空の chunked・HTTP/2・HTTP/3・トレーラ付きの POST）、`proxy_on_log` から
  見える応答のステータスとヘッダ、ログ・削除の段のトラップでも応答が届き「ログ済み」で二重に呼ばれないこと、本文の段の Pause の 503（B-300）、
  上限を超える chunked の 413、`Expect: 100-continue`、ヘッダの段の間に本文が見えないこと、`max_execution_time_ms` が段ごとに効くこと、
  長時間の負荷でゲストの線形メモリが増え続けないこと（SDK の文脈の表）。
- 複数のモジュールのルートの段の順の試験（既存の E2E が通る）。io_uring と epoll の両方。

## 関連

- [F-327](F-327-proxy-wasm-path-cost.md)（親）・[F-319](F-319-wasm-per-request-fixed-cost.md)（連続呼び出し・`phases` を見送った理由）・F-298・B-299
- `docs/artifacts/proxy-wasm-perf/gemini/ans-01-host-path.md` の候補 1・5(d)、`ans-04-design-review.md` の 1・6
