# B-393: Linkerd の管理ポートがクライアントの宣言した長さの本文を上限なく溜め、桁あふれで同じ要求を繰り返していた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx の 2 回目のセキュリティスキャン（`CLAUDE-SECURITY-20261114-024333`）の指摘 F7・F30 から確認。

## 事象

- F7: `mesh::linkerd::admin::serve_h1` は、クライアントの `Content-Length` をそのまま `usize` にして、本文を
  捨てるために全部 `acc` へ読み込んでいた。上限も読み取りの期限も無い。リスナーは既定で 0.0.0.0:4191、認証無し・
  平文。
- F30: `head_end + body_len` はリリースビルドで桁あふれの検査が無く、`Content-Length = 2^64 - head_end` で 0 に
  戻る。ループの条件が偽になり、`drain(..0)` は何も消さず、同じ要求を解析・応答し続けて新しい入力を読まない。

## 対応

- `parse_admin_head` で要求の先頭を読み、本文は `MAX_ADMIN_BODY`（64 KiB）まで（超えたら 413）、
  `Transfer-Encoding`・数字でない・重複した `Content-Length` は 400 にする（`parse_content_length` の厳格な読み）。
  長さは上限の内側なので足し算は桁あふれしない。本文の読み取りに 60 秒の期限を付ける。

## テスト

- `mesh::linkerd::admin::tests::admin_head_bounds_the_body`（`--features full,mesh-istio,mesh-linkerd`）。
