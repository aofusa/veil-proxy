# F-331: Proxy-Wasm の層の要求ごとの段取りとホスト関数の費用を減らす

**状態: 完了（2026-11-26、feat/proxy-wasm-perf。[F-327](F-327-proxy-wasm-path-cost.md) の子。採らなかったものと理由は「実装」の後）**

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

ans-01 が挙げたもの（2026-11-26 にコードで確かめた）:

- 要求ごとの `Rc<RequestSessions>`（`WasmModules::for_request`）と `Vec<SessionEntry>` の確保。
- `InstancePool::with_local` が要求ごとにスレッドの表を `retain`（全ワーカー共有の `alive` の強参照数を読む）し、`generation` を読む。
  読むだけで共有の行へは書かないので、優先度は低い。
- `settle`: HttpArena のフィルタはローカル応答の後に Pause を返すので、毎回 `resolve_pending_http_calls_inline` の future を作って空の保留を
  確かめる（[F-329](F-329-wasm-single-fiber-entry-per-request.md) の後はローカル応答で終えた連続呼び出しでは通らない）。
- HTTP/1.1 のローカル応答の経路の `Instant::now()`（`h1.rs` のローカル応答の 2 箇所。アクセスログが無効でも読む）、`Delete` の前の `metering::arm`。

## 改修内容

1. 確保: 段ごとの `Box::pin`（B-240 の意図〈Wasm の無い経路の future を太らせない〉を保つ形で）、`RequestSessions` と `SessionEntry` の
   要求ごとの確保。
2. 検索と作り直し: モジュールの名前の検索を、ワーカーの写し（`WasmModules`・`WorkerEngine`）に解決済みの添字で持つ（エンジンの世代と結ぶ）。
   `wants_request_body` 等は借りるだけにする（この 1 箇所は [F-329](F-329-wasm-single-fiber-entry-per-request.md) の前に先に直す）。
   要求ごとの `from_utf8`（`src/proxy/h1.rs`・`h2.rs` の `host`・`path`・`method`。POST は本文の段でもう一度）を 1 回にする。
   `src/http3_server/request_task.rs` の `modules_to_apply.clone()`（全ワーカー共有の `Arc` なら共有の行への書き込み）を確かめて減らす。`set_request_line` は要求ごとに 1 回、クエリは使うときに切り出す。`recycle` は触ったものだけ戻す。
3. ホスト関数: `proxy_get_property` の標準のプロパティ（NUL 区切りの `request\0path` 等）をバイト列のまま照合する近道、許可の判定を
   ロードのときに作る表で引く（`*`・`request.*`・カスタムのプロパティは今の判定に落とす）、`guest_memory()` と `data_mut()` を 1 回にまとめる。
4. その他: `settle` の空の保留の近道、時刻の読み取りを `request_start_instant()` へ。`proxy_on_delete` の前の `metering::arm` は省かない
   （前の段が上限の近くまで走っても削除の段を走らせて context を漏らさないための安全弁。ans-04 の 5-1）。

## 危険

- 許可のプロパティの判定を表にするときに権限を広げない（今の判定との一致を単体試験で網羅する）。
- リロード（B-404・F-293）で古いエンジンの添字・インスタンスを使わない（添字はエンジンの同一性〈`Arc::ptr_eq`〉か世代と結ぶ）。
- `Box::pin` を減らすときに Wasm の無い経路の future を太らせない（B-240。`-Zprint-type-sizes` で確かめる）。
- `RequestSessions` を使い回すなら F-329 の「ログ済み」を戻す。

## 受け入れ条件

- aofish の交互 A/B で全負荷の 1 要求のユーザー時間が下がる（目安 0.3 µs 以上）。`alloc-stats` で 1 要求の確保の回数を記録する。
- プロパティの許可の単体試験、リロードの E2E、Proxy-Wasm の E2E が通る。

## 実装（2026-11-26）

d1f34994:

- 要求ごとの状態（`RequestSessions` の `Rc` と context の表）を使い回す。最後の参照を手放すとき（`WasmModules` の `Drop`。
  `Rc::get_mut` が取れるときだけ）に要求の終わりの片付け（`Drop` と同じ。全フィールドを分割代入で列挙する `reset`。
  `proxy_on_delete` していない context をインスタンスの次の利用で消す表へ積み、「ログ済み」・要求の行を戻す）をして、スレッドの
  空きの表（64 個まで）に置く（`RequestSessions::take`・`give_back`。ワーカーの終わりに `release_free`。B-118 と同じく
  スレッドローカルの破棄より前）。
- ゲストに見せるパス・メソッド・クライアントの IP・`:authority` の `Arc<str>` は、種類ごとにスレッドで直前に作ったものと同じ
  バイト列なら複製する（`wasm::integration::request_str`。メソッドは標準の 7 つとその他で別の持ち分。HTTP/1.1・HTTP/2・HTTP/3）。
  UTF-8 の検査は作るときだけ。256 バイトを超えるもの・UTF-8 でないものは持たない（ab4c0ead。長いパスを持ち続けない・持ち分を
  壊さない）。全ワーカー共有の値にはしない（参照カウントはワーカーの中だけ）。
- 要求ヘッダの段で、プールの新しい context に要求のメソッドとパスを置かない（`open_session_for_request`。呼び出し元がゲストを
  呼ぶ前に置く）。要求の行を置くのは要求ごとに 1 回。
- モジュールの表（`ModuleRegistry::modules`）を SipHash から乱数シード付きの foldhash へ。
- HTTP/1.1 のローカル応答の時刻は `request_start_instant()`（アクセスログ・メトリクスが無ければ読まない）。

a6b081cc:

- `HttpContext::recycle`・`keep_request_line_only` を `reset_values` 1 つに（全フィールドを分割代入で列挙。足したフィールドを
  戻し忘れたらコンパイルで分かる）。ログの段・本文の段に入るとき、要求の行（本文の段はクライアントの IP も）を入れ替えない。
  文字列は空でないときだけ空に戻す。
- `proxy_get_property`: よく使う 11 のパス（`request.path`・`url_path`・`method`・`host`・`scheme`・`protocol`・`query`・
  `source.address`・`response.code`・`plugin_name`・`plugin_root_id`）を NUL 区切りのバイト列のまま表で照合して名前を引く
  （表に無い形は今の `parse_path_into`）。許可の判定は近道でも同じ（`get_property_value` の中の `is_property_allowed`）。
  `PropertyPath::is_empty` は長さで見る（`from_utf8` をしない）。値と戻り値の書き込みは `guest_memory()`・`data_mut` 1 回
  （戻り値の場所の境界は 02c75549 で `checked_add` にした）。

`wants_request_body`・`wants_response_body` を借りるだけにする 1 箇所は [F-329](F-329-wasm-single-fiber-entry-per-request.md)
（2ac41b26）で先に直した。HTTP/3 の `modules_to_apply.clone()` は確かめた: 並びは F-318 のワーカーごとの写し
（`config::worker_modules`。固定応答も `FixedResponse::for_worker`）、要求の状態とエンジンは `Rc` なので、共有の行へは書かない
（変えない）。

### 採らなかったもの

- 段ごとの `Box::pin`（1）: 残す。外すと Wasm の無い経路の future が太る（B-240）。F-329 の後、ローカル応答の要求は段の
  入り口が 1 つ（`Box::pin` 1 回）。
- モジュールの名前を解決済みの添字で引く案（2）: 採らず foldhash にした。添字はリロード（B-404・F-293）でエンジンの世代と結ぶ
  仕組みが要る。F-329 の後はローカル応答の要求の検索は 1〜2 回で、プロファイルの SipHash（0.04 µs）は foldhash で消えた。
- 許可の判定をロードのときの表で引く案（3）: 採らない。今の判定（`allowed_properties` の線形走査。パターンは数個）を
  そのまま使い、権限を広げる危険を作らない。照合の近道は名前を引くところだけ。
- `settle` の空の保留の近道（4）: F-329 の後、ローカル応答で終えた続けて呼ぶ経路は `settle` を通らないので足さない。
- `proxy_on_delete` の前の `metering::arm`: 省かない（ans-04 の 5-1）。

### 試験

`instance_pool.rs`（`given_back_sessions_are_reset_and_reused`・`the_free_list_is_bounded_and_dead_slots_are_not_queued`）、
`integration.rs`（`request_strs_are_reused_only_for_the_same_bytes`・`methods_have_their_own_slots`・
`invalid_or_long_bytes_do_not_evict`）、`properties.rs`（`known_properties_match_the_parsed_names`・
`known_properties_still_need_permission`・`property_path_is_cleared_after_spilling`）、`tests.rs` の `f331_request_reuse_tests`
（`proxy_on_context_create` から `request.path`・`request.method` を読む。使い回すモジュールと使い回さないモジュール・
最後の複製を手放すまで使い回さない・ログの段まで行かなかった要求の context は次の利用で消す。同じモジュールの
`get_property_paths_permissions_and_bad_pointers` はホスト関数を通した表のパス・最後の NUL・`.` で繋いだ形・長さ付きの形・
許可の無いプロパティ・メモリの外の場所とアロケータの戻り値・読めないパス）・
`f318_recycle_tests`（`enter_log_and_enter_request_body_keep_only_the_request_line`）。

### 確保の回数（手元、`alloc-stats`、HttpArena の ABI の版のフィルタ、1 要求あたり）

| 負荷 | F-330 の後 | d1f34994 | a6b081cc + F-330 の本文の段のヘッダ |
|---|---|---|---|
| h1 GET | 16 | 10 | 10 |
| h1 POST（Content-Length） | 25 | 19 | 14 |
| h1 POST（chunked） | 26 | 20 | 15 |
| `/json/50?m=2` | 16 | 10 | 10 |
| h2c `/baseline2`（10 ストリーム） | 21 | 15 | 15 |

### 交互 A/B（aofish、`~/hp/bench.py`、3 巡の中央値、1 要求のユーザー + システムの CPU µs）

1 回目（`f330b` = 422c4721 → `f331a` = d1f34994）:

| 負荷 | io_uring・ABI | io_uring・SDK | epoll・ABI |
|---|---|---|---|
| h1 GET（`h1-wasm`） | 14.42 → 13.96 | 15.14 → 14.84 | 21.00 → 19.91 |
| h1 GET（HttpArena の混ぜ方、`h1-arena`） | 16.01 → 15.70 | 16.73 → 16.59 | 25.17 → 23.97 |
| h1 POST | 16.09 → 15.78 | 16.52 → 16.22 | 24.06 → 23.31 |
| h1 POST（chunked） | 16.49 → 16.04 | 17.37 → 17.29 | 25.22 → 25.37 |
| json-tls | 18.99 → 18.60 | 19.87 → 19.72 | 18.53 → 18.37 |
| h2 | 5.73 → 5.53 | 6.38 → 6.22 | 5.75 → 5.57 |
| h3 | 6.72 → 6.56 | 7.14 → 7.25 | 6.55 → 6.45 |

2 回目（`f331a` → `f331c` = ab4c0ead。a6b081cc と F-330 の本文の段のヘッダ〈40c88098〉を含む）:

| 負荷 | io_uring・ABI | io_uring・SDK | epoll・ABI |
|---|---|---|---|
| h1 GET（`h1-wasm`） | 13.97 → 13.68 | 14.60 → 14.56 | 19.73 → 19.54 |
| h1 GET（`h1-arena`） | 15.68 → 15.26 | 16.71 → 16.62 | 23.72 → 23.06 |
| h1 POST | 16.06 → 14.92 | 16.45 → 16.21 | 23.46 → 21.84 |
| h1 POST（chunked） | 16.28 → 15.79 | 17.15 → 16.79 | 25.17 → 23.89 |
| json-tls | 18.67 → 18.44 | 21.48 → 19.67 | 18.49 → 18.30 |
| h2 | 5.69 → 5.40 | 6.28 → 6.08 | 5.57 → 5.44 |
| h3 | 6.73 → 6.54 | 7.04 → 7.01 | 6.64 → 6.38 |

2 回の合計（同じ回の中で比べた差の和）は io_uring・ABI の版で h1 GET −0.75、POST −1.45、chunked −0.94、json-tls −0.62、
h2 −0.49、h3 −0.35 µs（`h1-arena` −0.73）。受け入れ条件の目安（全負荷で 0.3 µs 以上）は、io_uring・epoll の ABI の版の全負荷と
SDK の版の 5 つで満たし、SDK の版の h3（+0.11・−0.03 で計 +0.08）と `h1-arena`（−0.14・−0.09 で計 −0.23）は届かなかった
（SDK の版はゲストの中の確保と SDK の表の費用が大きく、veil の側の差が埋もれる。h3 の 1 回目の +0.11 は誤差の中）。SDK の
json-tls の 2 回目（−1.8）は wrk の揺れが大きい（同じ回の ABI の版は −0.23）。

プロファイル（`prof12.sh`、`f331a` → `f331c`、h1 POST）: veil のホスト関数 0.169 → 0.110 µs、`from_utf8` 0.097 → 0.034 µs、
veil のエンジン 1.091 → 0.985 µs。残る上位は `on_request_phases` の future 0.167 µs、`HttpContext::reset_values` 0.126 µs
（要求ヘッダの表の組ごとの解放）、`run_chain` 0.109 µs、`call_chain` 0.087 µs。

### レビュー（Gemini）

- `gemini/ans-09-review-tls-F-331a.md`（d1f34994 と 422c4721）: must-fix 無し。B2-1（`request_str` の長さの上限）と B5 の試験を
  ab4c0ead で直した。
- `gemini/ans-10-review-F-331b-F-330c.md`（a6b081cc・40c88098・ab4c0ead）: must-fix 無し。3-1（戻り値の場所の境界の `checked_add`）、
  5-1（本文の段のヘッダを置き換える・消す・表ごと置き換えるホスト関数の結合試験、`reset_values` の残りのフィールド、ホスト関数を
  通したプロパティの試験と `PropertyPath::clear`）、5-2（ログの段まで行かなかった要求の context の後始末、rustls の送信のバッファの
  上限を超える `write_all_vectored`）を 02c75549 で直した。

## 関連

- [F-327](F-327-proxy-wasm-path-cost.md)（親）・[F-318](F-318-wasm-per-request-shared-writes.md)・[F-319](F-319-wasm-per-request-fixed-cost.md)・B-178・B-240・B-404
- `docs/artifacts/proxy-wasm-perf/gemini/ans-01-host-path.md` の候補 5(a)・7・8・9・10
