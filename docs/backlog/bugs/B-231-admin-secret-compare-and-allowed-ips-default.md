# B-231: admin API の秘密の比較が定数時間でなく、認証失敗のレート制限が無く、`allowed_ips` が空だと全 IP を許す

**状態: 未修正（2026-10-27 起票。セキュリティレビュー SB-06）**

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

## 関連

- [B-149](B-149-linkerd-admin-shutdown-any-peer.md)（Linkerd の管理ポートの同種の既定）、[F-21](../features/structured-access-log-admin.md)
