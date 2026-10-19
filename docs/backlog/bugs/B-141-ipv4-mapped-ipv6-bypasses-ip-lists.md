# B-141: IPv4 射影の IPv6 のアドレスが、IPv4 の拒否リストをすり抜けた

**状態: 完了（feat/v080-release、2026-10-17。F-236 の監査の F8）**

## 症状

デュアルスタックのリスナー（`[::]:443`）で受けた IPv4 のクライアントは `::ffff:a.b.c.d` で見える。`CidrRange` は
アドレスの種類が違うと一致しないとしていたので、`denied_ips = ["10.0.0.0/8"]` が `::ffff:10.0.0.1` に効かなかった。

## 修正

照合の前に `IpAddr::to_canonical` で IPv4 に直す（`contains_addr`・文字列の `contains` の両方）。設定に射影の形
（`::ffff:192.168.0.0/112`）で書いた範囲は IPv4 の範囲として持つ。

## 試験

`config::net_policy::tests::ipv4_mapped_ipv6_matches_ipv4_ranges`、`legacy_bin_tests::cidr_tests::test_ipv4_mapped_ipv6`（期待を逆にした）。

## 関連

F-236。
