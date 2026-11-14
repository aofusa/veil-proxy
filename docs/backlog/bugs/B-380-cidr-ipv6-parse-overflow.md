# B-380: 区切りの多すぎる IPv6 の CIDR で `8 - 左 - 右` が桁あふれした

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx の 2 回目のセキュリティスキャン（`CLAUDE-SECURITY-20261114-024333`）の候補から確認。

## 事象

- `CidrRange::parse`（`[security] blocked_ips`、ルートの `allowed_ips`・`denied_ips`、設定の検証）は std の
  `Ipv6Addr` の解析に失敗すると簡易の解析（`parse_ipv6`）へ回していた。`1:2:3:4:5:6:7::8:9` のように `::` の両側の
  区切りが合わせて 8 を超えると `8 - 左 - 右` が桁あふれし、デバッグビルドは panic、リリースビルドは別のアドレス
  （上の例は `1:2:3:4:5:6:8:9`）として読んでいた。
- `CidrRange::contains`（文字列で照合する経路）も同じ簡易の解析へ回していた。照合に渡るのは接続の相手のアドレスを
  整形した文字列だけ（要求のヘッダの値は渡らない）なので、要求からは届かない。設定（TOML・VDS）の書き誤りが
  黙って別の範囲になる。

## 対応

- 簡易の解析を削り、std の解析だけを使う（正しい表記はすべて読める）。読めない表記は範囲にしない（検証で断る）。
  `contains` も `IpAddr` として読めない文字列は含まない。

## テスト

- `config::net_policy::tests::malformed_ipv6_is_rejected_without_overflow`。
