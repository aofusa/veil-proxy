# B-419: 静的配信（File のルート）が `//` を潰してファイルを解決し、狭いルートの制限を外れた要求に同じファイルを返していた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-17）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Route-level access controls on static File routes are bypassed with duplicate slashes」（HTTP/1.1・HTTP/2 の 2 件）
から確認。

## 事象

- 既定の `[security] path_normalization = "base"` は連続したスラッシュを潰さず、ルートは `//admin/x` のまま選ぶ
  （`/admin/*` に一致しない）。Proxy のルートは B-376 で `//` を潰さずに上流へ送るようにした。
- File のルートの解決（`proxy::sendfile::handle_sendfile`・`proxy::h2::h2_sendfile`・HTTP/3 の
  `H3BufferedTask::handle_sendfile`）は残りのパスの先頭の `/` を全部剥がしてディレクトリへ結合していた。
  `path = "/"` 等の広い File のルートの下に、`[route.security]`（`allowed_ips` 等）・`[route.jwt]`・Wasm で守った
  `/admin/*` の File のルートのファイルがあると、`GET //admin/x` が広いルートの検査だけで同じファイルを返した。
  `/static//css/a.css` で `/static/css/*` の制限を外れるのも同じ。

## 対応

- 3 つのプロトコルの File のルートで、パス（クエリより前）に空のセグメント（`//`）があれば 404 にする
  （`http_utils::path_has_empty_segment`）。`path_normalization = "merge_slashes"` ならルートを選ぶ前に潰れるので
  従来どおり配信される。
- 利用者ガイド（`routing.md` の英日）に書いた。

## テスト

- `http_utils::tests::static_path_empty_segment`（クエリの中の `//` は見ない）。
