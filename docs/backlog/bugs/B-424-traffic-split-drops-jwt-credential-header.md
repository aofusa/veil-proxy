# B-424: `[route.traffic] split` の分割先が `[route.jwt]` のヘッダを落とし、キャッシュが利用者ごとの応答を共有した

**状態: 修正済み（fix/security-scan-fable-max、2026-11-17）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Traffic-split backends drop the route's JWT credential header, so the cache shares per-user responses
(incomplete B-390 fix)」から確認。

## 事象

- B-390 で、`[route.jwt] header` が `Authorization` 以外のルートは、そのヘッダの付いた要求でキャッシュを素通りする
  ようにした（`config::loaded::route_credential_header` → `cache.credential_header`）。
- `route_traffic::resolve` は分割先の Backend をルートの複製から作るときに `r.jwt = None` にしていたので、分割先の
  キャッシュの `credential_header` は `None` になり、`x-api-token` 等で認証した利用者の応答（`private`・`no-store`
  の無いもの）が host・path のキーで保存され、別の利用者（`allow_missing = true` なら匿名）へ返り得た。

## 対応

- 分割先の複製でも `jwt` を残す（Backend の組み立てが `jwt` を見るのは資格情報のヘッダを決めるところだけで、
  JWT の検証は親の `TrafficPolicy` が 1 回だけ行う）。

## テスト

- `route_traffic::tests::split_backends_keep_the_route_jwt_header`（分割先の組み立てに `[route.jwt]` のヘッダが渡る）。
