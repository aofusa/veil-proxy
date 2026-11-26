# F-331: Proxy-Wasm の層の要求ごとの段取りとホスト関数の費用を減らす

**状態: 未着手（[F-327](F-327-proxy-wasm-path-cost.md) の子）**

## 現状

プロファイル（h1 GET、µs/要求）で veil の Proxy-Wasm の層が 0.98、ホスト関数が 0.39。上位は `on_request_headers_with_modules` の
`Box::pin` の future、`HttpContext::recycle`、`open_session_with`、`call_chain`、`on_log_with_modules` の future、`set_request_attrs`、
`run_chain`、`set_request_line`、`WasmModules::for_request`、`proxy_send_local_response` のヘッダの変換、`parse_path_into`。
コードで確かめたもの:

- 段ごとに `registry.loaded_module(&r.name)` が `HashMap<String, _>` を SipHash で引く（プロファイルの SipHash 0.04 µs）。
- `wants_request_body`・`wants_response_body` が `registry.get_module(&r.name)` で全ワーカー共有の `Arc<LoadedModule>` を複製して捨てる
  （POST のたびに共有の行へ 2 回書く。F-318 の取りこぼし。`src/wasm/engine.rs`・`src/wasm/registry.rs:423`）。
- ヘッダの段で `set_request_attrs` が `set_request_line` を呼んだ直後に `set_request` がもう一度呼ぶ（クエリの写しも 2 回）。
  後始末の段・本文の段でも呼ぶ（GET 3 回・POST 5 回）。
- `proxy_get_property` の `PropertyPath` が 128 バイトを 0 で埋め直し（2 回）、要素を足すたびと `is_empty()` のたびに `from_utf8` で検査し直す
  （`src/wasm/host/properties.rs`）。許すプロパティの判定は `Vec<String>` の線形走査。
- `request_path` 等が要求ごとの `Arc<str>`（`arcinner_layout_for_value_layout` 0.20%）。

ans-01 が挙げたもの（着手のときにコードで確かめる）: 要求ごとの `Rc<RequestSessions>` と `Vec<SessionEntry>` の確保、`InstancePool::with_local`
の `retain`（共有の `alive.strong_count()`・`generation` を毎回読む）、`settle` が保留の無いときも `resolve_pending_http_calls_inline` を通る、
HTTP/1.1 のローカル応答の経路の `Instant::now()`（アクセスログ・メトリクスが無効でも）、`Delete` の前の `metering::arm`。

## 改修内容

1. 確保: 段ごとの `Box::pin`（B-240 の意図〈Wasm の無い経路の future を太らせない〉を保つ形で）、`RequestSessions` と `SessionEntry` の
   要求ごとの確保。
2. 検索と作り直し: モジュールの名前の検索を、ワーカーの写し（`WasmModules`・`WorkerEngine`）に解決済みの添字で持つ（エンジンの世代と結ぶ）。
   `wants_request_body` 等は借りるだけにする。`set_request_line` は要求ごとに 1 回、クエリは使うときに切り出す。`recycle` は触ったものだけ戻す。
3. ホスト関数: `proxy_get_property` の標準のプロパティ（NUL 区切りの `request\0path` 等）をバイト列のまま照合する近道、許可の判定を
   ロードのときに作る表で引く（`*`・`request.*`・カスタムのプロパティは今の判定に落とす）、`guest_memory()` と `data_mut()` を 1 回にまとめる。
4. その他: `settle` の空の保留の近道、時刻の読み取りを `request_start_instant()` へ、`metering::arm` の省略（前の段が 1 エポック未満のときだけ）。

## 危険

- 許可のプロパティの判定を表にするときに権限を広げない（今の判定との一致を単体試験で網羅する）。
- リロード（B-404・F-293）で古いエンジンの添字・インスタンスを使わない。

## 受け入れ条件

- aofish の交互 A/B で全負荷の 1 要求のユーザー時間が下がる（目安 0.3 µs 以上）。`alloc-stats` で 1 要求の確保の回数を記録する。
- プロパティの許可の単体試験、リロードの E2E、Proxy-Wasm の E2E が通る。

## 関連

- [F-327](F-327-proxy-wasm-path-cost.md)（親）・[F-318](F-318-wasm-per-request-shared-writes.md)・[F-319](F-319-wasm-per-request-fixed-cost.md)・B-178・B-240・B-404
- `docs/artifacts/proxy-wasm-perf/gemini/ans-01-host-path.md` の候補 5(a)・7・8・9・10
