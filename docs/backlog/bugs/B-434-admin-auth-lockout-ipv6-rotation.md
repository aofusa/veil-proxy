# B-434: 管理 API の認証の失敗の上限が送信元のアドレス（/128）ごとで、IPv6 の /64 の中でアドレスを替えるだけで外れた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-17）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Admin API brute-force lockout is keyed on the full client address, so rotating IPv6 source addresses removes the
limit」から確認。

## 事象

- 管理 API（`[admin]`）の総当たりの対策（B-231。1 分に 10 回の失敗で、その IP は正しい秘密でも 401）は、失敗を
  `(ADMIN_AUTH_ZONE, 送信元のアドレスの文字列)` で数えていた。
- `allowed_ips` に IPv6 を含む構成（全許可の例 `["0.0.0.0/0", "::/0"]`・社内の IPv6 の接頭辞）では、SLAAC 等で
  /64 の中のアドレスを 10 回ごとに替えれば上限に達せず、秘密をネットワークの速さで推測できた。

## 対応

- 失敗は IPv6 なら /64 ごと、IPv4 射影は IPv4 のアドレスとして数える（`config::prometheus::admin_auth_key`）。
- 利用者ガイド（`admin-api.md` の英日）に書き、長い乱数の秘密を使うよう書いた。

## テスト

- `config::tests::admin_auth_failures_are_rate_limited_per_ip` に /64 の中で送信元を替える例・IPv4 射影の例を足した。
