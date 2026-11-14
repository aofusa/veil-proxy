# B-370: `/*` の無いルートのパスが部分パスの候補にならず、広いルートの設定で素通りしていた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx のセキュリティスキャン（`CLAUDE-SECURITY-20261113-030705` の F3。MEDIUM）で判明。

## 事象

ルートの索引（`routing::PathRouter::add_route`）は、`/*` で終わらないパス（`path = "/admin"`）を matchit に完全一致の
`/admin` としてだけ入れていた。照合（`matches_path_pattern`）・上流のパスの計算・ガイドはこの形をセグメントの接頭辞
（`/admin/users` も合う）として扱うのに、`/admin/users` は `/admin` のルートの候補にならず、後ろの catch-all（`/`）が選ばれて、
そのルートの `[route.security]`・`[route.jwt]` だけが掛かっていた（IP の制限・JWT を素通り）。

## 対応

- `/*` で終わらないパスは `p`・`p/`・`p/{*rest}` を matchit に入れる。`/` で終わるパス・入れられないパスは従来どおり
  接頭辞のリスト（`patterns`）で照合する。

## テスト

- `routing::tests::test_path_without_wildcard_is_a_prefix_candidate`（`/admin` と catch-all があるとき、`/admin/users` の
  候補が `matches_pattern` と一致して両方になる）。
