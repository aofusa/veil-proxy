# B-133: `type = "Respond"`・`type = "Redirect"` のルートが `[route.security]` を無視していた

**状態: 完了（feat/v080-release、2026-10-16。F-234 の行列の試験で発見）**

## 症状

`Backend::security()` が `Redirect` と `Respond` に対して既定の `SecurityConfig` を返していた。そのため、次のルートでは
`[route.security]`（`allowed_ips`・`denied_ips`・`allowed_methods`・`max_request_body_size`・`rate_limit_requests_per_min`）が
効かず、IP で絞ったつもりのルートにどこからでも届いた。HTTP/1.1・HTTP/2・HTTP/3 の全部で起きていた。

- `type = "Redirect"`（前から）
- `type = "Respond"` と、`[route.traffic]` が返す応答（フォルトの中断・CORS のプリフライト）・`[route.jwt]` の 401 / 403（F-219・F-220）

## 修正

- `Backend::Redirect` にルートのセキュリティ設定（`Arc<SecurityConfig>`）を持たせた。
- `FixedResponse` に `security` を持たせ、`load_backend` と `route_traffic::resolve` がルートの設定を付ける。
- `Backend::security()` はそれを返す。

## 試験

E2E `standalone_e2e::f234_protocol_matrix_same_decisions`（`allowed_ips = ["10.0.0.0/8"]` の Respond のルートが 3 経路とも 403、
`allowed_methods = ["POST"]` が 405）。

## 関連

F-219、F-220、F-234、B-132。
