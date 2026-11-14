# B-386: リロードの途中に古いルートの表で照合した結果が、新しい表のルートの添字として使われ得た

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx の 2 回目のセキュリティスキャン（`CLAUDE-SECURITY-20261114-024333`）の指摘 F24 から確認。

## 事象

- `routing::RouteCache` の実体はスレッドローカルの LRU で、全体の `CACHE_GENERATION` だけで世代を見ていた。
- `reload_config` は `clear_cache`（世代を進める）を `CURRENT_CONFIG.store` より前に呼ぶ。その間に古いルートの表で
  照合した要求は、古い表の添字を新しい世代で保存する。公開の後、`select_route` はその添字を新しい表の `routes`
  に当て、そのルート自身の条件だけを確かめるので、新しい表でその前にある厳しいルートを飛ばし得た。
  キャッシュの鍵（ホスト・パス・メソッド）は攻撃者が決められる。

## 対応

- `RouteCache` ごとに世代（`generation`）を持つ。スレッドのキャッシュは、どの `RouteCache` の世代で作ったかを覚え、
  違う世代の `RouteCache` から引かれたら空にする。古い表の結果を新しい表で引くことはない。
- 設定ごとに `OptimizedRouter` は 1 つなので、定常状態で表が入れ替わってキャッシュが空になることは無い。

## テスト

- `routing::tests::route_cache_entries_belong_to_their_table`。
