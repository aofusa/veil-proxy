# B-390: 別のヘッダから JWT を読むルートで、利用者ごとの応答がキャッシュで共有されていた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx の 2 回目のセキュリティスキャン（`CLAUDE-SECURITY-20261114-024333`）の指摘 F13 から確認。

## 事象

- キャッシュの資格情報の検査（`request_has_authorization`）は `authorization` のヘッダだけを見ていた。
- `[route.jwt]` はトークンを任意のヘッダ（`header`。既定は `authorization`）から読める。別のヘッダを使うルート
  では、利用者 A の応答がホスト・パス・ルートだけの鍵で保存され、JWT の検査を通った他の利用者（`allow_missing = true`
  なら匿名の利用者も）に A の応答が返っていた。

## 対応

- `CacheConfig.credential_header`（設定のロード時に `[route.jwt] header` から決める。`authorization` なら無し）を
  足し、`bypass_for_authorization` はそのヘッダの付いた要求もキャッシュを引かず保存もしない（`key_headers` に
  入れたときは利用者ごとのキャッシュとして使う）。要求ごとの費用は、ルートに JWT のヘッダがあるときのヘッダの
  走査 1 回だけ。
- 利用者ガイド（`compression-cache.md`・`security.md` の英日）に書いた。

## テスト

- `cache::config::tests::bypass_for_route_credential_header`。
