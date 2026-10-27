# B-231: admin API の秘密の比較が定数時間でなく、認証失敗のレート制限が無く、`allowed_ips` が空だと全 IP を許す

**状態: 修正済み（v0.10.0）**

## 出典

2026-10-27 のセキュリティレビュー（v0.9.2 の `src/config/prometheus.rs::AdminConfig::check_auth` の読解）。

## 事象

- `check_auth` の Bearer の秘密の比較は `==`（コメントで「信頼ネットワーク前提」と明記）。タイミングで 1 バイトずつ当てられる。
- 認証失敗のレート制限が無い（`[route.security]` のレートリミットは admin のパスには掛からない）。
- `allowed_ips` が空だと全 IP を許す。`/metrics` も同じ既定。`[admin].enabled` と `[prometheus].enabled` は既定で無効なので、
  有効にした運用者が `allowed_ips` を書き忘れたときだけ露出する。

## 改修案

- 比較を定数時間に（`subtle::ConstantTimeEq`。`subtle` は rustls 経由ですでに依存にある。または長さの比較 + `ct_eq` 相当の自前実装）。
- `allowed_ips` が未設定なら loopback だけ許す既定に変える（互換性のため `allowed_ips = ["0.0.0.0/0"]` で明示的に全許可できる）。
  変更点を `docs/guide/`（英・日）・`examples/config.toml`・リリースノートに書く。
- 失敗のレート制限は既存の `RateTable`（`(ルート, IP)` のアトミック表）で admin のパスにも数える。

## 受け入れ条件

- 単体: 秘密の比較が長さの違いで早期に抜けない。`allowed_ips` 未設定で外部 IP からの admin・/metrics が 403。
- E2E: `allowed_ips` を書いた既存の設定が変わらず動く。

## 修正（v0.10.0）

- `AdminConfig::check_auth` の秘密の比較を定数時間に（長さが違っても途中で抜けない `constant_time_eq`）。
- `check_auth_from(client_ip, auth)` を足して HTTP/1.1・HTTP/2 の管理 API の 4 か所で使う。IP ごとの認証の失敗をレート表（`RATE_TABLE`、ゾーンは `usize::MAX`）で数え、1 分あたり 10 回を超えた IP は正しい秘密でも拒否する（読むだけの `rate_peek` を足した）。
- `[admin] allowed_ips`・`[prometheus] allowed_ips` が空のときは loopback（IPv4 射影を含む。UDS のリスナーの接続は 127.0.0.1）だけを許す（以前は全 IP）。全許可は `["0.0.0.0/0", "::/0"]`。Helm・Kubernetes の同梱設定は `allowed_ips` を書いているので変わらない。
- 試験: `admin_ip_allowed_empty_allows_loopback_only`・`admin_auth_failures_are_rate_limited_per_ip`。
- `examples/config.toml`・`docs/guide/admin-api.md`・`observability.md`（英日）を更新。挙動の変更はリリースノートの移行の注意に書く。

## 関連

- [B-149](B-149-linkerd-admin-shutdown-any-peer.md)（Linkerd の管理ポートの同種の既定）、[F-21](../features/structured-access-log-admin.md)
