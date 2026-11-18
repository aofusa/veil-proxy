# B-433: HTTP/3 の Retry トークンのタグがアドレスの族を区別せず、IPv6 の送信元に出したトークンを IPv4 の送信元の期限の無いトークンへ作り直せた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-17）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「QUIC Retry token HMAC input has no address-family separation」から確認。

## 事象

- `http3_server::admission::token_tag` は `ラベル || アドレス（IPv4 は 4 バイト・IPv6 は 16 バイト）|| ポート || 期限 ||
  元の DCID の長さ || 元の DCID` に HMAC を掛けていた。鍵はプロセスで 1 つで、全部の HTTP/3 のリスナーが共有する。
- IPv6 の送信元（下位のバイトを選べる者）に出したトークンの入力は、（IPv4 のアドレス・ポート・期限・元の DCID）と
  切り直せた。期限の欄が攻撃者のアドレスのバイトになり、プロセスの再起動まで使える IPv4 の送信元のトークンになる。
  IPv4 と IPv6 のリスナーを別に持つ構成で、送信元を偽った IPv4 の Initial がアドレスの検証（Retry）を通った。

## 対応

- アドレスは常に 16 バイト（IPv4 は IPv4 射影）でタグの入力に入れる（入力の切れ目が一意に決まる）。

## テスト

- `http3_server::admission::tests::token_for_ipv6_peer_does_not_validate_for_ipv4_resplit`（指摘の作り直しを再現）。
