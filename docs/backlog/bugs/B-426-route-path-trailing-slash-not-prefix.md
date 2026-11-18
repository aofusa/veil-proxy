# B-426: 末尾が `/` のルートのパス（`/admin/`）がそのパスにしか一致せず、下のパスがより広いルートへ落ちて検査を外れた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-17）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Route path patterns that end in '/' match only that exact path, so requests below them fall through to
less-restricted routes」から確認。

## 事象

- `path = "/admin/"` のような末尾が `/` のパターンは、ルーター（`routing::PathRouter`）の前方一致の照合
  （`matches_pattern`）へ回るが、`"/admin/users".strip_prefix("/admin/")` の残り `users` が `/` で始まらないので
  一致しなかった。ルートの選択の照合（`upstream::matches_path_pattern`）も同じ。
- `/admin/users` は次に一致するルート（多くは `/` の catch-all）へ行き、`/admin/` のルートの `[route.security]`・
  `[route.jwt]`・Wasm の検査が掛からなかった。ガイドは `"/static/"` が `/static/css/style.css` に一致する例を
  載せていた（B-370 で `/admin` を前方一致にしたときの取りこぼし）。

## 対応

- 両方の照合で、末尾の `/` を 1 つ除いてからセグメントの境界での前方一致にする（`/admin/` は `/admin`・`/admin/`・
  `/admin/...` に一致し、`/administrator` には一致しない）。
- 利用者ガイド（`routing.md` の英日）の `path` の説明に書いた。

## テスト

- `routing::tests::test_path_without_wildcard_is_a_prefix_candidate`・
  `upstream::tests::test_b96_root_path_pattern_matches_every_path` に末尾が `/` のパターンを足した。
