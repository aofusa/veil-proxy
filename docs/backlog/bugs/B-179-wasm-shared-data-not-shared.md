# B-179: Proxy-Wasm の shared data（`proxy_set_shared_data`・`proxy_get_shared_data`）が要求をまたいで共有されない

**状態: 未着手**

## 出典

B-175（Proxy-Wasm の経路の性能）の調査で `HttpContext::new` を読んでいて見つけた（2026-10-26）。

## 事象

- `src/wasm/context.rs` の `HttpContext::new` が `shared_data: Arc::new(RwLock::new(HashMap::new()))` を要求（コールバック）ごとに
  新しく作る。`src/wasm/host/shared_data.rs` の `proxy_set_shared_data`・`proxy_get_shared_data` はこの表を読み書きするので、
  値はその http context の中でしか見えない。CAS（`shared_data_cas`）も context ごと。
- Proxy-Wasm の shared data は同じ `vm_id` の VM（Envoy ではワーカーをまたぐ）で共有するキー・値の表で、レート制限の
  カウンタ・設定の配布・キャッシュ等に使う。veil では set した値を次の要求で get すると `NOT_FOUND` になる。
  `allow_shared_data` を許したフィルタが期待どおりに動かない（エラーにはならないので気づきにくい）。
- 要求ごとに `Arc` と `RwLock` の確保が 1 回ずつ増える（使わないフィルタでも）。

## 修正案

- 表をモジュール（`LoadedModule`）またはエンジンに 1 つ持ち、`HttpContext` には `Arc` の clone を渡す（要求ごとの確保が無くなる）。
  CAS もその表に持つ。
- データプレーンから全ワーカーで共有するので、同期ロック待ちを避ける形にする（読み取りは `ArcSwap` のスナップショット、
  書き込みは CAS で差し替え、またはキーごとの分割と短い臨界区間。ホットパス規則）。
- 設定のリロードで表を引き継ぐか（Envoy は VM を作り直しても shared data を残す）を決める。
- 試験: 1 つ目の要求で set、2 つ目の要求（別のスレッドを含む）で get できる。CAS の不一致で `CAS_MISMATCH`。
