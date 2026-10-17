# B-138: メッシュのルート・RBAC・JWT が、正規化していないパスで照合していた

**状態: 完了（feat/v080-release、2026-10-17。F-236 の監査の F5）**

## 症状

メッシュはパスの照合（ルート・RBAC の `paths`・JWT の規則）を生の要求のパスで行い、xDS の HCM の `normalize_path`・
`merge_slashes`・`path_with_escaped_slashes_action` を読んでいなかった。上流のフレームワークは普通パスを正規化するので、
`/%61dmin/users`・`/./admin/users`・`/public/../admin/users` で `/admin/*` の DENY を素通りできた。

## 修正

`src/mesh/path_norm.rs`: 予約されていない文字の `%XX` の復号・`\` を `/` に・RFC 3986 §5.2.4 のドットのセグメントの除去・
（`merge_slashes` なら）連続するスラッシュの統合・`%2F`/`%5C` の扱い（そのまま・400・復号）。
`l7::request_filters` の最初に 1 回だけ行い、正規化したパスでルート・RBAC・JWT を照合し、上流へも送る。

- 既定は正規化する（`normalize_path` を書かない HCM・Linkerd・ブートストラップ）。Istio は常に `normalize_path: true` を送る。
- 正規化の要らないパスは 1 回の走査だけで、確保しない。拒否は 400（応答のフラグ `DPE`）。

## 試験

`mesh::path_norm::tests`。

## 関連

F-236、B-137。
