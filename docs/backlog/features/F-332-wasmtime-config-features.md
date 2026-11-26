# F-332: wasmtime の設定と cargo の機能を Proxy-Wasm の経路に合わせて見直す

**状態: 未着手（[F-327](F-327-proxy-wasm-path-cost.md) の子。測って効いたものだけ入れる）**

## 現状

- Linux は crates.io の wasmtime 36.0.21。cargo の機能は `async`・`gc`・`gc-drc`・`gc-null`・`wat`・`parallel-compilation`・`cranelift`・
  `pooling-allocator`・`demangle`・`addr2line`・`coredump`・`debug-builtins`・`runtime`・`component-model`・`threads`・`stack-switching`・`std`・
  `pulley`（`Cargo.toml:793`。BSD の vendoring 版も同じ）。veil の `src/wasm/` は component・共有メモリ・コアダンプ・GC の参照・
  スタックの切り替えを使っていない。
- エンジンの設定（`src/wasm/registry.rs` の `create_engine`）はプーリングアロケータ、`async_support`、`epoch_interruption`、`cranelift_opt_level(Speed)`、
  スタックの大きさのみ。`table_lazy_init` 等は既定。
- ans-01（Gemini）の指摘: ホスト関数の呼び出しごとの `Caller::with` が GC のルートの範囲（`gc_roots().enter_lifo_scope()`）を出入りする、
  `table_lazy_init`（既定 true）で SDK の `Box<dyn HttpContext>` の `call_indirect` ごとに初期化の検査が入る。
- ans-02（`docs/artifacts/proxy-wasm-perf/gemini/ans-02-wasmtime.md` の 5。`Caller::with` の GC の範囲はソースで確かめた）:
  - `gc`・`gc-drc`・`gc-null`: ホスト関数の呼び出しごと（`run_chain` も）に `Caller::with` が GC のルートの範囲に出入りし
    （`wasmtime-36.0.21/src/runtime/func.rs` の `enter_lifo_scope`・`exit_gc_lifo_scope`。無効なら何もしない）、`TypedFunc::call_raw` が
    `AutoAssertNoGc` を 2 回作る。エンジンは GC の収集器（DRC）を作り、ストアとプールに GC のヒープの場所を持つ。veil は GC の参照を使わない。
  - `coredump`: トラップの準備（`CallThreadState`）ごとに 1 語の写しが増えるだけ。
  - `component-model`・`threads`・`stack-switching`・`debug-builtins`: 呼び出しごとの費用は無い（`stack-switching` を外してもスタックの鎖の
    保存と復元は消えない）。ストアとプールの構造が大きくなる・バイナリの大きさ。
  - `addr2line`・`demangle`: トラップのときのバックトレースの表示だけ。`wat`: 試験とテキストのモジュールの読み込みが使う。
  - `table_lazy_init`（既定 true）: `call_indirect` ごとの初期化の検査。h1・json-tls の主な費用ではない。

## 改修内容（候補。測ってから決める）

1. `gc`・`gc-drc`・`gc-null`・`coredump`・`component-model`・`threads`・`stack-switching`・`debug-builtins` を外して測る（ans-02 の見込みは
   −0.05〜0.15 µs/要求）。`gc` を外すと GC の型（`externref`・`anyref` 等）を使うモジュールは読めなくなる（Rust・TinyGo・C++ の Proxy-Wasm の
   SDK は使わない。`gc` 無しのとき wasmtime は `WasmFeatures::GC_TYPES` を閉じるだけで〈`wasmtime-36.0.21/src/config.rs`〉、これは `externref`・`anyref` 等を止め、`funcref` は止めない〈`wasmparser-0.236.1/src/features.rs`〉。reference-types の複数のテーブルも読める）。
   外すなら利用者の確認を得て、ガイドに書く。`addr2line`・`demangle`・`wat` は残す。
2. `instance_reuse` のときの `table_lazy_init(false)`。
3. その他、1 要求の費用に効く `Config` の項目（ans-02 の結果から）。

## 危険

- AOT のキャッシュ（`.cwasm`）の名前はモジュールのハッシュだけで、エンジンの設定を含まない（`src/wasm/registry.rs` の `aot_cache_path`）。
  設定の違う `.cwasm` は wasmtime の検査で読めずに作り直して上書きするので、A/B は 2 つのビルドでキャッシュの場所を分ける。版を上げた後の
  最初の起動は作り直しになる（名前に設定のハッシュを混ぜるかは、このチケットで決める）。
- BSD・aarch64 向けの vendoring 版（`veil-proxy-wasmtime`、`third_party/wasmtime`、Pulley）でも同じ機能の組でビルドでき動くこと
  （`full-freebsd`・`full-openbsd`・`full-netbsd` のビルド）。
- 外した機能を使うモジュールの読み込みが分かりやすいエラーで失敗すること。

## 受け入れ条件

- 入れたものそれぞれに aofish の交互 A/B（同じビルドの中の切り替えができないものは 3 巡）。
- Proxy-Wasm の E2E（試験のモジュール全部が読める）、BSD 向けのクロスビルドが通る。

## 関連

- [F-327](F-327-proxy-wasm-path-cost.md)（親）・B-55（vendoring 版）・`docs/artifacts/proxy-wasm-perf/gemini/ans-01-host-path.md` の候補 3・`ans-02-wasmtime.md`
