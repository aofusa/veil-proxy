# B-376: `path = "/"` のルートが `//admin/x` を上流へ `/admin/x` として送り、`/admin/*` のルートの制限を素通りできた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx の 2 回目のセキュリティスキャン（`CLAUDE-SECURITY-20261114-024333`）の候補から確認。

## 事象

- 上流へ送るパスはルートの接頭辞（`extract_path_prefix`。`path = "/api/*"` なら `/api`、`path = "/"` なら `/`）を
  剥がしてから上流の `path_prefix` を前に付けて作る（`compute_upstream_path`・`ws_upstream_path`・HTTP/3 の
  `compute_upstream_request_path`）。
- `path = "/"` は全部のパスに一致するルート（`any_path`）で、接頭辞は `/`。`//admin/x` は `/admin/*` のルートに
  一致しない（既定の `path_normalization = "base"` は連続する `/` を 1 つにしない）ので `path = "/"` のルートへ行き、
  先頭の `/` を剥がされて上流には `/admin/x` が届いていた。`/admin/*` の `[route.security]`・`[route.jwt]`・
  IP の制限を掛けずに、守っているはずのパスを上流から取れる。
- `path = "/api/"` のように末尾が `/` の接頭辞で `/api//x` を受けたときも同じ（上流には `/x`）。

## 対応

- 剥がす部分から接頭辞の末尾の `/` を除く（`proxy::route_strip_prefix`。HTTP/3 も同じ）。残りのパスは先頭の `/` を
  保ったまま上流の `path_prefix` に付くので、`//admin/x` は `//admin/x`（`path_prefix = "/v1"` なら `/v1//admin/x`）の
  まま届く。二重の `/` の無い要求の結果は変わらない（`/x` → `/x`・`/v1/x`、`/` → `/`・`/v1/`）。
- 連続する `/` を照合の前に 1 つにしたいときは従来どおり `path_normalization = "merge_slashes"`。

## テスト

- `proxy::path_tests::root_route_keeps_double_slash_upstream`、`http3_server::tests::root_route_keeps_double_slash_upstream`。
- 文書: `docs/guide/routing.md`（英日）の「Proxy (Proxy Pass Behavior)」。
