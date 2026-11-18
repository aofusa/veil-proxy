# B-440: キャッシュの例のフィルタが Vary・Authorization・Cookie を見ず、利用者ごとの応答を他の利用者へ返していた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Shared response cache ignores Vary, Authorization and Cookie, so one user's authenticated responses are served to anyone」から確認。

## 事象

- examples/wasm-filters/cache-filter のキーはメソッド・パス・`key_headers`（既定は Accept・Accept-Encoding）だけで作り、応答の `Vary` は読んで保存するだけでキーにもヒットの判定にも使っていなかった。`Authorization`・`Cookie` の付いた要求の応答も他と同じく保存した。
- `GET /api/me` が呼び出した者のプロフィールを `Vary: Authorization` で返す API の前に置くと、Alice の応答が保存され、TTL の間は資格情報の無い攻撃者の同じ要求に上流の認可を通さずに返った。

## 対応

- `Set-Cookie` の応答・`Vary: *` の応答は保存せず、`Authorization`・`Cookie` の付いた要求の応答は `Cache-Control` に `public`・`s-maxage` があるときだけ保存する（RFC 9111 §3.5）。`Vary` の各ヘッダの要求の値を一緒に保存し、ヒットは同じ値の要求にだけ返す（違えばミスとして上流へ）。キーに `:authority` を入れる。README に書いた。

## テスト

- `tests::shared_cache_rules`。
