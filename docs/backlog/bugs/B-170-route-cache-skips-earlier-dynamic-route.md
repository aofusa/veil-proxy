# B-170: ルートのキャッシュが、前にある header・query 条件のルートを飛ばす

**状態: 未着手（2026-10-25 起票。F-271 の E2E で判明）**

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
