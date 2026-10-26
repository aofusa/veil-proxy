# B-175: Proxy-Wasm の `instance_reuse` のプールが小さく、fuel の細かい yield で同時に開くインスタンスが溢れて要求ごとに作り直す

**状態: 修正済み（v0.10.0）**

## 出典

F-269（HttpArena の veil 単体の提出物＝Proxy-Wasm のフィルタが `type = "Respond"` のルートで答える）の計測で、`/json/{count}` が
nginx の 12.95k rps に対して 1.87k rps、veil の API ライブラリの提出物の半分だった（2026-10-26）。

## 事象

ローカルで veil を CPU 0-1、h2load（`--h1 -c 128`）を CPU 2-3 に固定して `/json/50?m=1` を測ると 2.7k rps（CPU 767 µs/req）。
`perf`（特権コンテナ）で見ると、時間の大半が JSON の組み立てではなくインスタンスの作り直しだった
（`instantiate_async`・プーリングアロケータの decommit の `madvise`・メモリイメージの `mprotect`・ページフォルト・
`_start`/`proxy_on_configure`・フィルタの起動時のデータセットの解析）。

原因は 2 つの組み合わせ:

1. `store.fuel_async_yield_interval(Some(10_000))`: 燃料 10,000（約 10µs）ごとにコールバックが yield する。JSON を組み立てる
   コールバック（数百 µs）は何十回も yield し、その間に同じワーカーの他の要求のコールバックが始まる。1 ワーカーで同時に開いている
   インスタンスが接続数近くまで増える。
2. `InstancePool` は 8 区画 × 2 個（`PER_SHARD = 2`）の固定の上限で、戻せない分は捨てていた。同時に開いているインスタンスが
   上限を超えると、次の要求は `_start`・`on_configure` から作り直す（`instance_reuse` が効かない）。

`PER_SHARD = 256` だけで 8.5k、yield 間隔 1,000,000 だけで 9.5k、両方で 9.5k rps（210 µs/req）。

加えて要求ごとの確保が残っていた（`HttpContext::new` の `Arc::from("")` × 3・権限の `ModuleCapabilities` の deep clone
（`Vec<String>` を含む）・モジュール名の `String` の clone、ローカル応答の本文の `clone()`）。

## 修正

- `src/wasm/engine.rs`: fuel の async yield の間隔を設定キー `[wasm.defaults] yield_interval_us`（既定 1,000 ≒ 1ms。fuel は
  × 1,000）にする（以前は 10,000 fuel ≒ 10µs の固定）。CPU の上限は今までどおり `fuel_limit`、実時間の上限は B-173 のエポック
  （約 1 秒）。CPU を長く使うフィルタは既定で約 1ms ごとに同じワーカーの他の要求へ譲る（下の実施内容の遅延の計測）。
  遅延をより詰めたい構成は値を下げられる（要求ごとの費用と引き換え）。
- `src/wasm/instance_pool.rs`: プールをワーカーのスレッドごとの持ち分にし（B-177 でロックも外した）、上限を
  `per_thread_capacity`（プーリングアロケータの `total_memories` の半分を、`instance_reuse` のモジュールの数とワーカーの数で割った数。
  下限 2）にする。上限を超えて作ったインスタンスはその要求だけが使う（`total_memories` の残りの半分）。
- 要求ごとの確保を減らす: `HttpContext` の権限は `Arc<ModuleCapabilities>`、モジュール名は `Arc<str>`、空の文字列は共有の
  `Arc<str>`。プールのインスタンスのローカル応答は `take()` で取り出す（本文をコピーしない）。

フィルタ側（HttpArena の提出物）も `serde_json::to_vec` を要求ごとの手書きの書き出し（エスケープ済みの文字列を起動時に用意し、
スレッドローカルの `Vec` に書く。nginx のモジュールと同じ）にした。出力はバイト単位で同じ。veil の変更ではない。

## 試験

- 既存の `wasm::` の単体テスト（F-271 の `instance_reuse_tests`、B-173 の `b173_other_calls_do_not_consume_an_armed_deadline`、
  fuel の上限の試験）がそのまま通る。
- `instance_pool::tests`（`per_thread_capacity` の計算）。
- 計測（下の実施内容）。

## 実施内容（2026-10-26）

交互 A/B（bcbfae6c の `veil-base2` と修正後の `veil-v1`、2 巡で順序を入れ替え）。veil は CPU 0-1・`threads = 2`、h2load は
CPU 2-3・`--h1 -c 128 -t 2`、3 秒の暖機の後 10 秒。`cpu_us_per_req` は veil の utime+stime を成功した要求の数で割った値。
負荷平均 3〜4（他のエージェントのビルドが同居）。

| 版 | フィルタ | `/json/50?m=1` rps | µs/req | `/baseline11` rps | µs/req |
|----|----------|-------------------:|-------:|------------------:|-------:|
| bcbfae6c | serde | 2,610 / 2,730 | 767 / 735 | 26,243 / 26,198 | 74.8 / 75.8 |
| bcbfae6c | 手書き | 2,697 / 2,687 | 742 / 744 | 26,318 / 26,028 | 75.4 / 75.6 |
| B-175 | serde | 9,787 / 9,509 | 205 / 205 | 27,209 / 26,478 | 72.7 / 73.5 |
| B-175 | 手書き | 16,785 / 16,901 | 117 / 117 | 24,545 / 26,954 | 74.4 / 73.3 |

- veil の修正で `/json` が 3.6 倍（CPU は 1/3.6）。フィルタを手書きにすると、修正前は作り直しに埋もれて変わらず（2.7k）、
  修正後は 1.7 倍。`/baseline11` は変わらない（作り直しは起きていなかった）。
- 残りの費用はフィルタの中（JSON の書き出しと wasmtime の `memory.copy`）と、要求ごとの http context の作成・削除。後者は B-177 で
  1 要求 1 context にした（コールバックごとに作っていた）。
