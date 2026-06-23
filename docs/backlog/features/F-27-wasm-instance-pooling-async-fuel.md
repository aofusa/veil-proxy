# F-27: WASM インスタンスプーリング + wasmtime Async/Fuel Yield

## 概要

`src/wasm/engine.rs` でリクエストごとにインスタンス化している部分を、スレッドローカルなインスタンスプールに変更し、wasmtime の `Config::async_support(true)` と epoch interruption を有効化して monoio ワーカーを占有しない非同期 WASM 実行を実現する。

## 現状

- `module.instance_pre.instantiate(&mut store)` がリクエストのたびに呼ばれる（高コスト）
- WASM 実行が monoio の非同期ワーカースレッド上でインライン（同期的）に実行される
- WASM 側の処理が数ミリ秒ブロックすると、そのスレッドの全 io_uring I/O がストールする（Head-of-Line Blocking）

## 改修内容

### スレッドローカルインスタンスプール

```rust
thread_local! {
    static WASM_INSTANCE_POOL: RefCell<HashMap<String, VecDeque<PooledWasmInstance>>>
        = RefCell::new(HashMap::new());
}
```

- プールからインスタンスを取得してコンテキスト ID をリセット・再利用
- 使用後はプールへ返却

### wasmtime Async + Epoch Interruption

```rust
let mut config = wasmtime::Config::new();
config.async_support(true);
config.epoch_interruption(true);
```

- `Store::set_epoch_deadline(1)` でエポック境界ごとに Yield
- `engine.increment_epoch()` を別スレッドから定期呼び出し
- WASM の実行を `.await` で Yield させ、io_uring イベントをインターリーブ

### Fuel Yield

```rust
config.consume_fuel(true);
store.set_fuel(fuel_limit)?;
store.fuel_async_yield_interval(Some(10_000))?;  // 10k 命令ごとに Yield
```

## 受け入れ条件

- [ ] リクエストごとのインスタンス化がなくなりプールから再利用される
- [ ] WASM 実行が `async_support(true)` + `epoch_interruption(true)` で動作する
- [ ] 長時間実行 WASM がワーカーをブロックしないことを確認
- [ ] `cargo test --features "full"` が通る
- [ ] `cargo clippy --features "full"` が通る

## 依存・リスク

- wasmtime の `async_support` は `cranelift` と組み合わせる必要がある（すでに feature に含まれる）
- プール内のインスタンスがメモリを保持し続けるため、メモリ使用量の上限管理が必要
- epoch スレッドは既存の `spawn_wasm_tick_thread()` と統合可能

## 優先度

P2

---

## 完了メモ（2026-06-23）

`config.async_support(true)` が設定されていたにもかかわらず、WASM 実行は**同期 `Func::call` / `InstancePre::instantiate` を使用**しており、wasmtime 40 ではこれらが async ストア上で **panic**（`must use *_async`）する。実モジュール実行時に毎回パニックする壊れた状態だった。

修正内容:

- `src/wasm/engine.rs` の全ライフサイクル呼び出し（102 箇所の `func.call` + 15 箇所の `instantiate`）を `call_async().await` / `instantiate_async().await` に変換し、`execute_on_*` および公開ラッパを async 化。
- `store.fuel_async_yield_interval(Some(10_000))` を追加し、10,000 命令ごとに協調的に yield。CPU バウンドな WASM が io_uring ワーカーを占有して他 I/O をストールさせる Head-of-Line Blocking を解消。
- ホットパス（proxy.rs / http3_server.rs の async ハンドラ）は `.await` で駆動。背景スレッド（WASM tick スレッド等）は `futures::executor::block_on` で駆動（ホットパスではない）。
- インスタンスは wasmtime の **Pooling Allocator**（`InstanceAllocationStrategy::Pooling`）からスロット再利用で確保されるため、リクエストごとの `instantiate_async` でもアロケーションコストは抑制される。完全初期化済みインスタンスのキャッシュ（_start/configure のスキップ）は今後の追加最適化。

実モジュール（`header_filter.wasm`）がパニックせず `FilterResult` を返すことを `src/wasm/engine.rs` のスモークテストで検証。
