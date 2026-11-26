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
- ans-02（wasmtime の調査）の結果をここに足す。

## 改修内容（候補。測ってから決める）

1. 使っていない機能の費用を測る（`perf annotate` で呼び出しごとの命令）。費用があり、veil が要らない機能は外す。ただし利用者のモジュールが
   読めなくなるもの（Rust 1.82 以降のモジュールは reference-types の符号化を使う。externref は `gc` が要る等）は外さない。
2. `instance_reuse` のときの `table_lazy_init(false)`。
3. その他、1 要求の費用に効く `Config` の項目（ans-02 の結果から）。

## 危険

- AOT のキャッシュ（`.cwasm`）は設定が変わると使えない（キャッシュの鍵に設定が入っているか確かめる）。
- BSD（vendoring 版・Pulley）でも同じ機能の組で通ること（`full-freebsd` 等のビルド）。
- 外した機能を使うモジュールの読み込みが分かりやすいエラーで失敗すること。

## 受け入れ条件

- 入れたものそれぞれに aofish の交互 A/B（同じビルドの中の切り替えができないものは 3 巡）。
- Proxy-Wasm の E2E（試験のモジュール全部が読める）、BSD 向けのクロスビルドが通る。

## 関連

- [F-327](F-327-proxy-wasm-path-cost.md)（親）・B-55（vendoring 版）・`docs/artifacts/proxy-wasm-perf/gemini/ans-01-host-path.md` の候補 3・`ans-02-wasmtime.md`
