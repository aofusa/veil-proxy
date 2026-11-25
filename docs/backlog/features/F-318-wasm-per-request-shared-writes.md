# F-318: Proxy-Wasm の要求ごとの「全ワーカーで共有する行への書き込み」を無くす（型レジストリのロック・共有の `Arc` の参照カウント）

**状態: 未着手（2026-11-22 起票。F-317 の子。HttpArena の Wasm のプロファイルが 0.3〜0.6M rps で頭打ちになる原因の本命）**

## 今の状態（測った値）

HttpArena（64 スレッド、v0.10.1 のイメージ）の 1 要求あたりの CPU（`cpu% × 10000 / rps`、µs）:

| プロファイル | veil（Proxy-Wasm） | veil-api（Rust のハンドラ） | nginx（C のモジュール） | veil の rps |
|---|---|---|---|---|
| baseline-4096 | 194.7 | 25.8 | 19.6 | 318k |
| limited-conn-4096 | 193.5 | 32.8 | 23.7 | 315k |
| pipelined-4096 | 104.7 | 19.0 | 15.8 | 597k |
| json-tls-4096 | 129.3 | 53.8 | 48.9 | 505k |
| baseline-h2-1024 | 108.0 | 14.1 | 19.3 | 582k |
| baseline-h3-64 | 75.6 | 13.6 | 6.3 | 526k |

ノート PC（4 CPU、veil-load、`docs/artifacts/benchmark/httparena-perf/results/m1.tsv`）の Wasm の上乗せは 1 要求 **約 25 µs**
（main: h1 12.3 → 37.1 µs、h2c 4.5 → 29.5 µs）。HttpArena では同じ上乗せが **約 60〜170 µs** で、コアが増えると膨らむ。
どのプロトコルでも 0.3〜0.6M rps で止まり、CPU は 62 コア分使い切っている（待ちではなく、命令が遅い）。
コアの数に比例して 1 要求の費用が増えるのは、**全ワーカーが同じキャッシュラインへアトミックに書く**ときの典型（ラインの持ち主が
コアの間を往復し、64 コアでは 1 回の書き込みが 100 ns 以上になる。1 本のラインが捌ける書き込みは毎秒数百万回で頭打ち）。

## 要求ごとに共有の行へ書いているところ（コードで確かめたもの）

1. **エクスポートの関数を要求・コールバックごとに引き直す**: `engine.rs` の `get_typed_func`（`open_session` の
   `proxy_on_context_create`、`proxy_on_request_headers`、`proxy_on_request_body`、`proxy_on_log`、`proxy_on_done`・`proxy_on_delete` 等）と、
   ホスト関数がゲストのメモリを取るたびの `get_export("proxy_on_memory_allocate")` + `typed::<i32, i32>()`
   （`host/buffers.rs`・`host/properties.rs`・`host/shared_data.rs` ほか）。wasmtime 36 の `Func::typed` は `load_ty` で
   **エンジンの型レジストリのロック**を取り、型（`RegisteredType`。`Arc` と登録数）を複製・破棄する（`func.rs` のコメント
   「requires taking a lock as well as cloning a type」）。1 回で共有の行への RMW が数回、1 要求で 8〜10 回呼ぶ。
   ロックと登録数はエンジンに 1 つで、全ワーカーが同じ行を書く。**これが本命**（1 要求で数十回、0.5M rps で毎秒 2 千万回を超える）。
2. **`HttpContext` の作り直し**: `close_session` が要求ごとに `HttpContext::for_module` を作る（前の文脈は破棄）。`with_shared_data` が
   全ワーカー共有の `Arc` を 9 個複製する: `empty_str()` × 4（`static OnceLock<Arc<str>>`）、`empty_configuration()` × 2、
   `empty_plugin_data()`、モジュールの `capabilities`・`shared_data`。破棄でまた減らす。
3. **要求の入口の `Arc` の複製**: `wasm_engine.clone()`（`Arc<FilterEngine>`）、`WasmModules::for_request(backend_modules.clone())`・
   `modules_to_apply.clone()`（ルートのモジュールの並び）、`on_request_complete_async(wasm_engine.clone(), modules_to_apply.clone())`。
   Wasm のルートだけ `CURRENT_CONFIG.load()` をもう 1 回読む（`proxy/h1.rs`・`proxy/h2.rs`・HTTP/3）。
4. その他の候補（要確認）: `persistent_context::allocate_context_id`（`SeqCst` の `fetch_add`）、`context::next_unique_id`、メトリクスの
   カウンタが要求ごとに通るか。

## 改修内容

1. インスタンスのスロット（`instance_pool::InstanceSlot`）を作るときに、使うエクスポートの `TypedFunc` を 1 回だけ引いて持つ
   （`proxy_on_context_create`・`proxy_on_request_headers`・`proxy_on_request_body`・`proxy_on_response_headers`・
   `proxy_on_response_body`・`proxy_on_log`・`proxy_on_done`・`proxy_on_delete`・`proxy_on_memory_allocate`。無いものは `None`）。
   ホスト関数は `HostState` に持たせた `proxy_on_memory_allocate` の `TypedFunc` を使う。`TypedFunc` は `Copy` で、呼ぶときに
   型の検査・ロックは無い。インスタンスを作り直さない経路（`instance_reuse = false`）も、インスタンスを作るときに同じ表を作る。
2. `HttpContext` を要求ごとに作り直さない: スロットに 1 つ持ち、要求の終わりに要求ごとのフィールドだけを空にする（`Vec` は
   `clear` で容量を残す）。空の値は共有の `Arc` の複製をやめ、`Option`（無い）で表すか、ワーカーごとの `thread_local` の値にする。
   モジュールの `capabilities`・`shared_data` はスロットを作るときに 1 回だけ複製する。
3. 要求の入口: エンジンとルートのモジュールの並びは借用で渡す（`&Arc` / `&FilterEngine`）。フューチャをまたぐため借用で
   渡せないところは、ワーカーごとの複製（設定の世代が変わったときだけ作り直す）を使う。`CURRENT_CONFIG.load()` は要求で 1 回
   （AGENTS.md「`CURRENT_CONFIG.load()` は 1 回」）。
4. 上の 4 の候補は呼ばれていれば、ワーカーごとの範囲で採番する（一意性は「ワーカー番号 + ワーカー内の連番」）。
5. 同じ観点の Wasm 以外の箇所（ルートのキャッシュの統計、ルートの `Arc` の複製、静的配信のキャッシュの `hits.fetch_add` 等）は
   [F-321](F-321-core-path-per-request-shared-writes.md) で扱う。伸び方の試験は両方に共通（Wasm の無いルートの値が F-321 の分）。

## 確かめ方（実装の前と後）

- **伸び方の試験**: aofish（16 スレッド。計測は WSL のロック `/tmp/vp-bench.lock` を取る）でワーカーを 1・2・4・8・12 本にし、
  Wasm のルート（`/baseline11`）と Wasm の無いルート（`/plain`）の 1 要求あたりの CPU（`/proc/<pid>/task/*/schedstat`）を測る。
  共有の書き込みが原因なら、改修前は Wasm のルートだけワーカーが増えるほど費用が上がり、改修後は平らになる。
- 可能なら `perf c2c`（aofish の特権の perf コンテナ）で、型レジストリのロック・`empty_str()` の `Arc` の行が HITM の上位に出るのを
  改修前に確かめる。
- 交互 A/B（ノート PC、h1・h2c、Wasm の GET・POST・`/json`）で単発の費用も悪化しない。

## 受け入れ条件

- 16 スレッド機で、Wasm のルートの 1 要求あたりの CPU が、ワーカー 1 本と 12 本とで ±15% 以内（改修前の値も記録する）。
- 単発の費用（ノート PC）が悪化しない。Proxy-Wasm の E2E（HTTP/1.1・HTTP/2・HTTP/3、epoll でも）・単体・`cargo test`。
- `instance_reuse = false`・リロード（モジュールの差し替え）・`max_execution_time_ms` の打ち切りの試験が通る。

## 期待する効果

- HttpArena の 6 プロファイル（baseline・limited-conn・json-tls・baseline-h2・baseline-h3・pipelined）。Wasm の上乗せがノート PC 並み
  （約 25 µs）まで下がれば、baseline は 1 要求 約 50 µs → 約 1.2M rps（今 318k）。F-317 の試算で総合点 +約 740（S1 → S2。
  2026-11-24 の結果〈okena を含む〉で計算）。

## 関連

- [F-317](F-317-httparena-infra-score-plan.md)（親）・[F-319](F-319-wasm-per-request-fixed-cost.md)・[F-321](F-321-core-path-per-request-shared-writes.md)・
  B-175・B-173・F-297
