# B-425: プロキシのキャッシュのキーが、`key_headers` のヘッダの 2 つ目以降と UTF-8 でない値を落とし、上流は全部を受け取っていた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-17）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Proxy cache key leaves out a keyed header (Cookie/Authorization) when its value is not UTF-8 or appears twice, but
the raw header still goes upstream」から確認。

## 事象

- `http_utils::extract_vary_headers_for_cache_key` は `key_headers` のヘッダの最初の 1 つだけをキーに入れ、値が
  UTF-8 でなければそのヘッダをキーから外していた。上流（`proxy::forward`）へは全部のヘッダをそのまま送る。
- `key_headers = ["Cookie"]` のルートで `Cookie: session=<攻撃者>; z=\xff` を送ると、キーは Cookie の無い要求と同じに
  なり、上流は壊れた欠片を無視して攻撃者のセッションの応答を返し、匿名の訪問者へ TTL の間それが返った。
  `Cookie: lang=en` の後に 2 つ目の `Cookie: session=<攻撃者>` を送っても、`lang=en` だけの利用者のキーへ入った。

## 対応

- `key_headers` に挙げたヘッダが 2 つ以上ある要求・値が UTF-8 でない要求は、キャッシュを引かず保存もしない
  （`http_utils::cache_key_headers_usable`。`key_headers` が空なら何もしない）。
- 利用者ガイド（`compression-cache.md` の英日）に書いた。

## テスト

- `http_utils::tests::cache_key_headers_must_be_single_utf8`。
