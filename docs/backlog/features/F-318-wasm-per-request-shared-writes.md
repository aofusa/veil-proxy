# F-318: Proxy-Wasm の要求ごとの「全ワーカーで共有する行への書き込み」を無くす（型レジストリのロック・共有の `Arc` の参照カウント）

**状態: 完了（2026-11-25、feat/httparena-perf 2dabe5c4・dceee430・39893767・bdf45609。2026-11-22 起票。F-317 の子。HttpArena の Wasm のプロファイルが 0.3〜0.6M rps で頭打ちになる原因の本命）**

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

## 結果（2026-11-25）

### 行ったこと

1. （2dabe5c4）インスタンスのエクスポートの関数を `TypedFunc` で引いたまま持つ（`wasm::exports::GuestExports`。ストアのデータに置き、
   呼ぶ間だけ取り出す。`TypedFunc` の複製も型の複製なので複製しない）。要求ごとの型レジストリのロックと型の複製が無くなった。
2. （dceee430）プールのインスタンスの `HttpContext` を作り直さない（`HttpContext::recycle`。要求ごとのフィールドだけ空にし、`Vec` の
   容量は残す。空の文字列はワーカーごとの `thread_local`。モジュールの `capabilities`・`shared_data`・設定は持ち続ける）。要求の属性
   （B-408）は `proxy_on_context_create` の前にストアの文脈へ置く。
3. （39893767）段ごとにモジュールと設定の `Arc` を複製しない（`resolve_effective_modules` は借用の反復子。ルートのモジュールの並びは
   ルートの表のワーカーの写しで作り直す〈`Backend::for_worker`・`FixedResponse::for_worker`〉）。エンジンの写しと設定の読み取りは B-501。
4. `persistent_context::allocate_context_id` は保留中の `http_call` の文脈だけで使い、要求ごとには呼ばれない（対象外）。
5. （bdf45609、改修 1・2 の後の perf で見つけた）ホスト関数がゲストのメモリを取る確保関数（`proxy_on_memory_allocate`）を、同期の
   ホスト関数から今のファイバーの上で呼ぶ（`Func::call_unchecked`）。以前は非同期のホスト関数から `call_async` で入れ子に呼んでいた
   （B-20）。wasmtime はストアに持ち越すファイバーのスタックが 1 本だけなので、入れ子のスタックを毎回プーリングアロケータ（全ワーカー
   共有の Mutex と添字の表）から取って返していた（h2c-wasm の CPU の約 1 割: `lock_contended` 3.3%・`index_allocator` 5.5%・
   `StoreFiber` の破棄 3.7%）。入れ子は 1 段（確保の中からの再入は確保の失敗）、ファイバーのスタック（2 MiB）が外側と入れ子の
   `max_wasm_stack`（512 KiB）× 2 とホストのフレームより大きいことをコンパイル時に検査する。確保の途中の fuel の区切り・取り消し・
   スタック溢れの試験を足した。

### 交互 A/B（aofish、サーバはワーカー 8 本・CPU 0〜7、負荷は CPU 8〜15、3 回、1 要求の CPU µs）

改修 1・2（f680dc43 → dceee430）:

| 経路 | epoll | io_uring |
|---|---|---|
| h1 の Wasm | 49.14 → 37.15（−24.4%、rps +35.9%） | 29.58 → 22.55（−23.8%） |
| パイプラインの Wasm | 20.42 → 12.43（−39.1%、rps +65.7%） | 19.83 → 11.14（−43.8%） |
| h2c の Wasm | 18.59 → 9.66（−48.0%、rps +78.2%） | 19.09 → 9.79（−48.7%） |
| h2 の Wasm | 19.26 → 10.51（−45.4%） | 19.77 → 10.89（−44.9%） |
| HTTP/3 の Wasm | 9.50 → 8.43（−11.3%） | 10.89 → 9.34（−14.2%） |
| JSON（TLS の h1） | 61.91 → 49.59（−19.9%） | 67.65 → 54.98（−18.7%） |

改修 3・5（dceee430 → bdf45609。改修 3 だけの 39893767 は −0.8〜−6.2% で揺れの範囲）:

| 経路 | epoll | io_uring |
|---|---|---|
| h1 の Wasm | 33.37 → 30.30（−9.2%） | 21.44 → 17.80（−17.0%、rps +17.7%） |
| パイプラインの Wasm | 12.66 → 8.85（−30.1%、rps +49.7%） | 11.18 → 7.86（−29.7%） |
| h2c の Wasm | 10.05 → 6.98（−30.5%、rps +54.6%） | 9.71 → 6.82（−29.8%、rps +52.5%） |
| h2 の Wasm | 10.97 → 7.96（−27.4%） | 10.82 → 8.03（−25.8%） |
| HTTP/3 の Wasm | 8.31 → 7.98（−4.0%） | 9.28 → 8.76（−5.6%） |
| JSON（TLS の h1） | 47.61 → 42.83（−10.0%） | 51.35 → 45.48（−11.4%） |

- Wasm の無い経路（h1・h2c の `Respond`）はどちらも ±2% の揺れの範囲。JSON の rps は wrk（WSL2）が TLS で 250 前後のエラーを出す回が
  あり比べられない（CPU は比べられる）。

### 伸び方（epoll、ワーカー 1・2・4・8 本、サーバの CPU も同じ本数、1 要求の CPU µs）

| 経路 | f680dc43（改修前） | dceee430（改修 1・2） | bdf45609（改修 1〜5） |
|---|---|---|---|
| h2c の Wasm | 5.54 / 6.13 / 10.15 / 19.10（3.45 倍） | 4.35 / 4.45 / 6.17 / 9.90（2.28 倍） | 3.88 / 3.90 / 5.20 / 6.87（1.77 倍） |
| h2c の `Respond`（対照） | — | — | 1.55 / 1.51 / 1.80 / 2.40（1.55 倍） |

- 16 スレッドの機械でサーバ 8 本・負荷 8 本を走らせると、Wasm の無い経路も 1 → 8 本で 1.55 倍になる（ハイパースレッドの兄弟を負荷と
  分け合う）。対照で割ると Wasm の経路の伸びは改修前 2.2 倍 → 改修 1〜5 で 1.14 倍。受け入れ条件（1 本と 12 本で ±15%）は、この機械では
  12 本にすると負荷と同じ CPU に載るので 8 本までで測り、対照に対して +14%。
- 残りの伸びの候補: wasmtime の `call_async` はファイバーを作るたびにエンジンの `Arc` を複製して捨てる（全ワーカー共有の行への加算・
  減算。1 要求に 4 回）。wasmtime を変えずに減らすには、続けて呼ぶコールバックを 1 回のファイバーの出入りにまとめる（F-319）。

### 見つけたこと

- h1 の Wasm は epoll で io_uring より 1 要求の CPU が多い（30.3 vs 17.8 µs）。`/proc/<pid>/task/*/stat` で分けると、epoll はユーザ
  13.5・システム 17.6 µs、io_uring は 8.8・9.5 µs（Wasm の無い h1 は epoll 2.3・8.2、io_uring 2.2・7.3。h2c の Wasm はどちらも
  6.4・0.5）。差の大半はシステム時間で、ループバックでは応答の `send(2)` が負荷ツールのスレッドを起こす費用を払う（Wasm で遅くなると
  負荷ツールのスレッドが眠っている割合が増える。ワーカー 1 本の h1 の Wasm が 47.8 µs と高いのも同じ）。io_uring は送信をまとめて
  提出するので起こす回数が少ない。h2c は 1 回の書き込みで複数の応答を送る。Wasm の経路の固有の費用ではないので、ここでは扱わない。

## 関連

- [F-317](F-317-httparena-infra-score-plan.md)（親）・[F-319](F-319-wasm-per-request-fixed-cost.md)・[F-321](F-321-core-path-per-request-shared-writes.md)・
  B-175・B-173・F-297
