# B-371: stale-while-revalidate の裏の更新がクライアントの Host・転送ヘッダをそのまま上流へ送り、その応答を保存していた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx のセキュリティスキャン（`CLAUDE-SECURITY-20261113-030705` の F4。MEDIUM）で判明。

## 事象

`server::spawn_background_revalidation` は、上流への要求を中継と別に組み立てていた。

- `Host` はクライアントの値（中継は上流の `target.host`）。仮想ホストで分ける上流は別のサイトの応答を返し、それを
  共有のキャッシュへ保存する（キャッシュの汚染）。
- パスは `path_prefix` を付けない（中継と別のパスを要求する）。
- ヘッダはクライアントの全部（hop-by-hop・`remove_request_headers` で消すもの・信頼しない送信元の `X-Forwarded-For` を含む）。
  `add_request_headers`・転送ヘッダの方針（B-232）・Via も無い。

## 対応

- 中継のヘッダを書く部分を `proxy::forward::write_upstream_request_headers` に切り出し、裏の更新は
  `build_revalidation_request` で中継と同じに組み立てる（上流の Host・`path_prefix`・ヘッダの方針・クライアントの IP の
  転送ヘッダ・`Connection: close`）。クライアントの IP を裏の更新へ渡す。
- 中継の経路の処理は変えていない（同じ関数を `#[inline(always)]` で呼ぶ）。

## テスト

- `proxy::forward::revalidation_request_tests::revalidation_request_follows_the_forwarding_policy`。
- 文書: `docs/guide/compression-cache.md`（英日）の stale-while-revalidate。
