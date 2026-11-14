# B-379: ルートの `source_ip` が IPv4 射影の IPv6 のクライアントに一致せず、確認の経路は IPv6 の CIDR を見ていなかった

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx の 2 回目のセキュリティスキャン（`CLAUDE-SECURITY-20261114-024333`）の候補から確認。

## 事象

- デュアルスタックのリスナー（`listen = "[::]:443"`。Linux の既定は `bindv6only=0`）で受けた IPv4 のクライアントの
  アドレスは `::ffff:a.b.c.d` になる。ルーター（`routing::CidrMatcher`）はこれを IPv6 として比べ、IPv4 の
  `source_ip`（`10.0.0.0/8`・完全一致）に一致しなかった。`source_ip` で特定の送信元を拒否・制限するルートへ
  振り分ける構成では、その送信元が別のルートへ行けた。ルートの `[route.security]` の `allowed_ips`・`denied_ips`
  （`CidrRange::contains_addr`）と全体の `blocked_ips` は F-236 で IPv4 として比べるようになっていた。
- ルートのキャッシュに当たったときの再確認と線形の安全網（`upstream::matches_cidr`）は IPv4 の CIDR しか見ず、
  IPv6 の CIDR の条件は常に一致しなかった。`/0` は `1 << 32` で桁あふれした（デバッグビルドでは panic）。
  ルーター側も `/33` 以上の接頭辞で `32 - prefix_len` が桁あふれした。

## 対応

- ルーター・ルートのキャッシュの鍵・再確認のいずれも `IpAddr::to_canonical` で IPv4 射影を IPv4 にしてから比べる
  （設定の完全一致の項目も同じ）。
- 再確認は IPv6 の CIDR を扱い、マスクは `checked_shl` で作る（`/0` は全部に一致）。
- アドレスより長い接頭辞の項目は読み込みで警告を出して無視する（照合で桁あふれしない）。

## テスト

- `routing::tests::cidr_matcher_canonicalizes_ipv4_mapped_peers`、`upstream::tests::matches_cidr_handles_ipv6_mapped_and_edge_prefixes`。
- 文書: `docs/guide/routing.md`（英日）の `source_ip`、`examples/config.toml`。
