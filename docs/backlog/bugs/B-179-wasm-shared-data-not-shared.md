# B-179: Proxy-Wasm の shared data（`proxy_set_shared_data`・`proxy_get_shared_data`）が要求をまたいで共有されない

**状態: 修正済み（v0.10.0）**

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

## 実施内容

- `src/wasm/shared_data.rs` に `SharedData`（キーのハッシュで 16 区画に分けた `RwLock<HashMap>`、値は `Arc<[u8]>`、CAS は
  表ごとの `AtomicU32` で払い出す）と、モジュール名 → 表のプロセス全体の登録 `for_module` を置く。登録を引くのはモジュールの
  ロード（起動・リロード）のときだけで、`LoadedModule.shared_data` に持つ。リロードで同じ名前のモジュールを作り直しても同じ表を
  返す（Envoy は VM を作り直しても shared data を残す）。
- 要求の http context は `HttpContext::for_module` で作り、権限と表の `Arc` を clone するだけ（要求ごとの `Arc`・`RwLock` の
  確保が無くなった）。`HttpContext::new` はモジュールに属さない context（試験）用で、自分だけの表を作る。
- `proxy_get_shared_data` は値の `Arc` を返して 1 回だけゲストのメモリへ写す。`proxy_set_shared_data` は表で CAS を検査して置く。
  ロックは 1 回のハッシュ表の操作の間だけ持ち、`await` をまたがない（同じ区画への書き込みが重なったときだけ短く待つ）。
- 表のキー数には上限を設けない（Envoy と同じ。値の大きさは従来どおり `max_shared_data_size`）。
- 文書（`docs/guide/wasm.md`・ja）の `allow_shared_data` に共有の範囲を書く。

## 試験

- `wasm::tests` の `b179_shared_data_is_shared_per_module_name`（同じ名前は同じ表・別スレッドから同じ値・名前が違えば別の表）、
  `test_shared_data_cas_increment`（古い CAS では `CAS_MISMATCH`）。
