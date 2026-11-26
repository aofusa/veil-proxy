# F-332: wasmtime の設定と cargo の機能を Proxy-Wasm の経路に合わせて見直す（見送り）

**状態: 見送り（2026-11-26、利用者の判断。[F-327](F-327-proxy-wasm-path-cost.md) の子。拡張性を性能より優先する方針により、veil の wasmtime の
cargo の機能と設定は変えない）**

## 見送りの理由

- 利用者の方針（2026-11-26）: Proxy-Wasm は拡張性を大事にする。Envoy の Proxy-Wasm で動くフィルタ・Rust の Proxy-Wasm の SDK で書いたフィルタが
  そのまま動くことを重視し、今できることができなくなるのは避ける。veil 本体の機能は減らさない。HttpArena 向けの最適化はフィルタ（拡張の側）で
  行う（[F-334](F-334-httparena-filter-and-guide.md)）。
- 1 要求の費用に効くのは `gc`・`gc-drc`・`gc-null` だけで（ans-02 の見込みは他と合わせて −0.05〜0.15 µs/要求、試算で 10〜15 点）、外すと
  Wasm 2.0 の `externref` を使うモジュールが読めなくなる。他の機能は外しても 1 要求の費用は変わらない（ans-02 の 5）。

## 機能ごとの調べ（記録）

wasmtime 36.0.21 の既定の Wasm の機能（`src/config.rs` の `features()`）は Wasm 2.0 に multi-memory・relaxed-simd・tail-call・extended-const・
memory64 を足したもので、`GC_TYPES`・`THREADS`・`COMPONENT_MODEL` は cargo の機能があるときだけ有効になる。veil の `create_engine`
（`src/wasm/registry.rs`）は Wasm の機能を上書きしていない。

| cargo の機能 | 今できること | 外したときに失うもの | 1 要求の費用（ans-02 の 5） |
|---|---|---|---|
| `gc`・`gc-drc`・`gc-null` | `GC_TYPES`。Wasm 2.0（reference-types）の `externref` 等を使うモジュールが読める（GC の提案〈struct・array〉自体は `wasm_gc` が既定で無効なので今も読めない） | `externref` を使うモジュール（Proxy-Wasm の SDK は使わないが、他の言語・道具の出力に含まれ得る） | ホスト関数ごとの GC のルートの範囲の出入り（`Caller::with`）、呼び出しごとの `AutoAssertNoGc` 2 回 |
| `threads` | `THREADS`。アトミック命令・共有メモリを使うモジュールが検証を通る（共有メモリはプーリングアロケータが拒む〈`memory_pool.rs:333`〉ので、プールを切った設定でだけ動く） | アトミック命令・共有メモリを使うモジュール | なし |
| `component-model` | `COMPONENT_MODEL`。veil は今コンポーネントを読まない | 将来コンポーネントのフィルタを扱う余地 | なし（ストアの構造が大きくなるだけ） |
| `stack-switching` | 今も使えない（`wasm_stack_switching` が既定で無効） | なし | なし（スタックの鎖の保存と復元は 36 では外しても消えない） |
| `coredump` | トラップのときのコアダンプ（veil は今使っていない） | 障害の解析の手段 | トラップの準備ごとに 1 語の写し |
| `debug-builtins`・`addr2line`・`demangle` | ネイティブのデバッガへの登録、トラップのバックトレースの表示 | 障害の解析の手段 | なし |
| `wat` | テキスト形式（`.wat`）のモジュールを読める（`Module::new`）。試験も使う | `.wat` のフィルタ・試験 | なし |

- 機能を変えない設定の候補だった `table_lazy_init(false)`（`call_indirect` ごとの初期化の検査を無くす）も入れない。効果は小さく（ans-02 の 5-2。
  h1・json-tls の主な費用ではない）、エンジン全体の設定なので、インスタンスを使い回さないモジュールの作成が重くなる。
- HttpArena のエントリだけ機能を減らした veil を使う案も採らない（エントリで動く veil の機能が減る。効果も小さい）。
- 線形メモリの読み書きの境界の検査は、今の設定（4 GiB の予約・32 MiB のガード。既定のまま）ですでに省かれている（ans-02 の 6）。
  フィルタの `put_i64` に見える検査は、ゲストの Rust のスライスの検査（フィルタの側。[F-334](F-334-httparena-filter-and-guide.md)）。

## 関連

- [F-327](F-327-proxy-wasm-path-cost.md)（親）・B-55（vendoring 版）・`docs/artifacts/proxy-wasm-perf/gemini/ans-01-host-path.md` の候補 3・`ans-02-wasmtime.md` の 5・6
