# B-22: パスワイルドカード `/x/*` が境界パス `/x`・`/x/` を取りこぼす

## 事象

ルート条件 `path = "/api/*"` を設定したとき、`/api/v1/users` のような深いパスは
マッチするが、境界パス **`/api`（末尾スラッシュなし）** と **`/api/`（末尾スラッシュ）**
がマッチせず、意図しない別ルート（catch-all 等）へフォールバックする。

## 再現手順

```rust
let mut router = PathRouter::new();
router.add_route(0, Some("/api/*"));
assert!(router.get_candidates("/api").contains(&0));  // ← 修正前は失敗
assert!(router.get_candidates("/api/").contains(&0)); // ← 修正前は失敗
```

F-56 で追加したプロパティテスト `tests/routing_proptest.rs::path_wildcard_prefix_boundary`
（`rest=""` の最小反例）で検出。

## 調査メモ

- `PathRouter::add_route` はパラメータ（`:`/`{`）を含まない `/api/*` を
  `convert_pattern` で matchit 形式 `/api/{*rest}` へ変換し radix ツリーへ登録する。
- matchit のキャッチオール `{*rest}` は **1 セグメント以上**を要求し、**空セグメント**
  （`/api` や `/api/`）にはマッチしない。
- 一方、fallback の `matches_pattern("/api/*", path)` は
  `path.len() == prefix.len()`（＝`/api`）や次バイトが `/`（＝`/api/…`）を許容する
  **正しいプレフィックス境界意味論**を持つ。
- ワイルドカードパターンは matchit 経路にのみ入り fallback を通らないため、
  **同じ `/api/*` でも「matchit に入るか fallback に入るか」で意味論が食い違う**
  一貫性バグだった。

## 改修

- `PathRouter::add_route`: matchit 登録に成功したパターンが `"/*"` で終わる場合、
  **fallback `patterns` にも登録**し、境界パス（`/api`・`/api/`）を
  `matches_pattern` の正しい意味論で拾えるようにした。深いパスは matchit と fallback の
  両方に一致するが、候補は `OptimizedRouter::get_candidates` の `dedup` で重複排除される。
- 語境界（`/apix` は非マッチ）は `matches_pattern` の既存ロジックで維持。

## テスト

- 回帰単体テスト `routing::tests::test_path_wildcard_boundary_matches_base_and_trailing_slash`。
- プロパティテスト `tests/routing_proptest.rs::path_wildcard_prefix_boundary`
  （境界一致 + 語境界外の非一致を任意入力で検証）。

## 影響

- 優先度 P2。catch-all ルートがある構成では実害が軽減されるが、`/api/*` 配下に
  固有バックエンドを割り当てた構成で `/api`・`/api/` が誤ルーティングされていた。

## 関連

- 検出元: [F-56](../features/F-56-property-load-tests.md)（プロパティテスト）。
