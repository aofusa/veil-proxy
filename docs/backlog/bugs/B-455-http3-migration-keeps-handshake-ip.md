# B-455: HTTP/3 の接続が別の IP へ移っても、IP で見る検査はハンドシェイクの IP のまま行っていた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「HTTP/3 IP-based access decisions stay bound to the handshake address after the QUIC peer migrates to another
address」・「HTTP/3 IP access decisions use the handshake-time source address and are never re-evaluated after QUIC
peer migration」（2 件）から確認。

## 事象

- `http3_server::event_loop::process_datagram_segments` は、確立した接続の接続 ID を持つパケットを送信元を問わず
  quiche へ渡していた。vendoring 版の quiche のサーバは、veil が `disable_active_migration` を送っていても、新しい
  送信元から届いた非プロービングで最大の番号のパケットで接続をそこへ移す（`on_peer_migrated`）。パスのイベントは
  読み捨てていた（B-256）。
- 接続の `peer_addr`・`client_ip` は最初の Initial の送信元のまま変わらないので、`[security] blocked_ips`（新しい
  Initial でしか見ない）・ルートの `allowed_ips`/`denied_ips`・`source_ip` の条件・Prometheus の `allowed_ips`・
  IP ごとのレート制限・アクセスログ・`X-Forwarded-For` は、別の IP から届く要求をハンドシェイクの IP として扱った。
  許された IP でハンドシェイクだけ済ませれば、断られる IP から接続が続く限り要求を通せた。

## 対応

- 接続の IP と違う IP から届いたパケットは、quiche へ渡す前（復号の前）に捨てる（RFC 9000 §9 が
  `disable_active_migration` の違反に許す扱い。偽の送信元で接続を切らせることもない）。ポートだけの変化
  （NAT の付け替え）は従来どおり受け、移った先のポートを `peer_addr` に写す（IP は同じなので検査は変わらない）。
- 利用者ガイド（`protocols.md` の英日。「接続マイグレーション」の行が実際の扱いと違っていた）に書いた。

## テスト

- `http3_server::tests::test_b455_packets_from_another_ip_do_not_migrate_the_connection`（別の IP のパケットは
  quiche へ届かず、パスは 1 本のまま。同じ IP のポート違いは受けて `peer_addr` が新しいポートになる）。
