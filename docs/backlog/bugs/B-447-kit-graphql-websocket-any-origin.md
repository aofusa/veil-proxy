# B-447: kit の GraphQL の WebSocket が別のオリジンのハンドシェイクも受け、別のサイトのページが利用者の資格情報で GraphQL を実行できた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「veil_kit::graphql::mount accepts GraphQL-over-WebSocket upgrades from any Origin (cross-site WebSocket hijacking;
side-steps the B-196 GET/POST CSRF rules)」から確認。

## 事象

- `veil_kit::graphql::mount`（Rust の kit）は WebSocket のハンドシェイクで `WebSocketUpgrade::origin` を設定せず、
  `mount` にも設定の手段が無かった。受けたソケットは `async_graphql::http::WebSocket` に渡り、購読だけでなく
  問い合わせ・mutation も実行する。
- ブラウザは別のサイトの WebSocket のハンドシェイクにも Cookie・Basic 認証の資格情報を付け、WebSocket は同一
  オリジンポリシーの外なので、別のサイトのページが利用者として任意の GraphQL を実行し、結果も読めた（B-196 の
  GET・POST の規則の迂回）。既定の `127.0.0.1:8080`・イントラネットのサービスなら資格情報なしでも同じ。
- Python の kit（`veil_kit.graphql.mount`）の WebSocket も Origin を見ていなかった。

## 対応

- Rust の kit: `Origin` の付いたハンドシェイクは、`Host` と同じホスト・ポート（既定のポートは省略を同じとみなす）か、
  新しい `mount_with_origins(path, schema, &[..])` で並べたオリジンのときだけ受け、他は 403。`Origin` の無い
  ハンドシェイク（ブラウザ以外）は従来どおり受ける。
- Python の kit: 同じ規則（`mount(..., origins=[...])`）。断るときはハンドシェイクを 403 で返す。
- C の helpers の GraphQL は WebSocket を利用者のハンドラへ回すので、`veil_ws_options_t` の `origins` を設定するよう
  ガイドに書いた。
- 利用者ガイド（`library-kit.md`・`library.md` の英日）に書いた。`Cors` のプラグインは WebSocket に効かないことも書いた。

## テスト

- `veil_kit::graphql::tests::websocket_origin_is_checked`（判定と、`execute` での 403・101）。
- `crates/veil-python-kit/tests/test_kit.py` の `test_websocket_origin`。
- `tests/api/library_e2e.sh` の「graphql WebSocket cross-site Origin is 403」。
