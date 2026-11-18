# B-429: ルートのレート制限が `[route.traffic] split` の分割先ごとに数えられ、上限が分割先の数の倍になった

**状態: 修正済み（fix/security-scan-fable-max、2026-11-17）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Per-route rate limit is multiplied by the number of traffic-split targets」から確認。
後の検証の「Per-route rate limit is split across [route.traffic] split destinations, so clients get several times the configured limit」も同じ指摘。

## 事象

- レート制限の表は `(枠, IP)` で数え、枠は Backend のセキュリティ設定のアドレス（`rate_limit_zone` があればそれ）。
- `route_traffic::resolve` は分割先ごとに Backend を組み立て（設定は分割先ごとに別の `Arc`）、枠を揃えていなかった
  ので、`rate_limit_requests_per_min = 10` のログインのルートに 2 つの分割先があると、1 つの IP から毎分約 20 回
  通った（B-31 の「ルート × IP」の不変条件に反する。CORS の写しは F-273 で枠を共有している）。

## 対応

- 分割先の設定の `rate_limit_zone` を、ルートの Backend の枠（無ければ最初の分割先の枠）に揃える。
- 利用者ガイド（`routing.md` の英日）の `rate_limit_requests_per_min` に書いた。

## テスト

- `route_traffic::tests::split_backends_share_the_route_rate_limit_zone`。
