# B-170: ルートのキャッシュが、前にある header・query 条件のルートを飛ばす

**状態: 完了（wt/core → feat/v0100、2026-10-25。2026-10-25 起票。F-271 の E2E で判明）**

## 現象

同じ path の 2 つのルートで、前（先に書いた方）に `header`（または `query`）の条件を付けると、条件の無い後ろの
ルートに一度当たった後は、条件に合う要求も後ろのルートに行く。

```toml
[[route]]           # 0: x-mode: fresh のときだけ
[route.conditions]
path = "/a"
header = { "x-mode" = "fresh" }

[[route]]           # 1: それ以外
[route.conditions]
path = "/a"
```

`x-mode` 無しの `/a`（ルート 1 に当たる）の後、`x-mode: fresh` の `/a` もルート 1 に当たる（first-match ならルート 0）。

## 原因（調査で確認）

`src/upstream.rs` の `find_backend_unified` の Phase 4 は、`RouteCacheKey`（host・path・method・送信元）で前回当たったルートの
添字を引き、そのルートの条件に合えばそれを返す。キャッシュの鍵に header・query が入らないので、前にある動的な条件のルートを
評価し直さない。

## 直し方の案

- キャッシュに入れるのは、当たったルートより前の候補に header・query の条件が無いときだけにする（設定ロード時にルートごとに
  「前に動的な条件のルートがあるか」を求めておけば、要求ごとの費用は増えない）。
- 単体試験（上の 2 ルートで順に引く）と、HTTP/1.1・HTTP/2・HTTP/3 の E2E。

## 直したこと（2026-10-25）

- `src/upstream.rs` のルートの照合を `select_route`（`OptimizedRouter` を引数で受ける。単体試験のため）に分け、候補を
  順に見る間に「前の候補が header・query の条件（`has_uncached_conditions`）や Backend の組み立ての失敗で外れたか」を
  持ち、外れたものがあれば当たったルートをキャッシュに入れない。前の候補が無い・動的な条件を持たないときは今までどおり
  入れる。安全網の線形探索（`find_backend_linear`）も同じ（host・path で外れたかは見ず、条件を持つだけで数える）。
- 案の「設定ロード時にルートごとに求める」は採らなかった。候補（host・path・送信元で絞った後）の中の前の候補だけを見る
  ほうが正確で（関係の無い host・path の header ルートでキャッシュが効かなくなることが無い）、費用も候補を見るループの
  中の `Option` の確認 2 つだけ。キャッシュに当たったときの経路は変えていない。
- 試験: 単体 `upstream::tests::test_b170_route_cache_keeps_earlier_dynamic_routes`（header・query・条件なしの 3 ルートを
  順に引く。直す前は失敗）、E2E `b170_route_cache_keeps_earlier_header_and_query_routes`（HTTP/1.1・h2c・TLS の HTTP/2・
  HTTP/3 で、条件なしの要求の後の header・query の要求が前のルートに行く）。

