# B-301: Proxy-Wasm の起動時のコールバック（`_start`・`on_vm_start`・`on_configure`・コンテキストの作成）の結果を見ず、ロード時にも走らせない

**状態: 修正済み（2026-11-05、v0.11.0）**

## 事象（コードの読み取り）

- 結果を `let _` で捨てている（`src/wasm/engine.rs:236-268,364-370,1035-1077`）。モジュールはロード時・リロード時に一度も走らないので、`on_configure` が
  false を返す（設定が不正）モジュールもそのまま要求に使われる。Envoy はその設定を拒否する。
- 再利用しないモジュールでは、起動時のコールバックと要求のコールバックが同じ fuel の予算を使う（`max_execution_time_ms` の説明と合わない）。

## 改修案

- 起動・リロード・ECDS の適用で、モジュール × ルートの設定ごとに `_start` → `on_vm_start` → `on_configure` を 1 回走らせ、trap か false なら設定を拒否する
  （起動は失敗、リロードは前の設定のまま、ECDS は NACK）。要求時の結果も見て、false・trap は失敗としてポリシー（[F-293](../features/F-293-wasm-failure-policy.md)）。
- 起動時のコールバックの予算を要求のコールバックと分ける（または文書を直す）。
- 試験: `on_configure` が false を返す WAT のモジュールで起動が失敗し、リロードでは前の設定が残る。

## 修正

- 起動時のコールバック（`_start`・`proxy_on_context_create`・`on_vm_start`・`on_configure`）の結果を見る（トラップ・false は `lifecycle` の失敗）。13 か所の重複を `run_lifecycle` 1 つにした。
- `wasm::init`（起動・リロード）とメッシュの ECDS の適用で `FilterEngine::validate` を走らせ、失敗したら設定を拒否する。
- 起動時のコールバックの後に燃料を入れ直し、要求のコールバックの予算を分けた。

## 関連

- [F-293](../features/F-293-wasm-failure-policy.md)
