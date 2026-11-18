# B-441: キャッシュの例のフィルタのキーが 64 ビットの FNV-1a とキーの先頭 15 バイトで、別の要求で同じキーを作って共有の項目を差し替えられた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Cache key relies on a 64-bit FNV-1a hash plus a 15-byte prefix, so an attacker can forge a colliding key and poison
a shared cache entry」から確認。

## 事象

- examples/wasm-filters/cache-filter の `generate_cache_key` は、キーの文字列（メソッド・パス・キーのヘッダを区切り文字で
  つないだもの）の FNV-1a（64 ビット）と先頭 15 バイトの base64 をキーにしていた。FNV-1a は各段を逆算できるので、
  先頭の同じ別のパスで同じ値を中間一致で作れた。キーのヘッダの値は 100 バイトで切っていた。
- 攻撃者は被害者のキー（`GET /products/123` 等）と衝突する自分のパスの応答を、項目の期限切れの後に共有の
  Redis・Memcached へ入れ、以後の全員に返させられた。

## 対応

- キーは、メソッド・ホスト・パス・キーのヘッダの名前とすべての値を長さを前に付けて並べたものの SHA-256
  （`veil:cache:v2:<16 進>`）にする（値は切り詰めない）。依存に `sha2` を足した（jwt-filter と同じ版）。README に書いた。

## テスト

- `tests::cache_key_separates_fields`（examples/wasm-filters/cache-filter）。
