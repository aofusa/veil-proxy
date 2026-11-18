# B-445: Linkerd の inbound が自分の inbound のポートへ中継し、送信元を Pod 自身の IP に見せてネットワークでの認可を外せた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Linkerd inbound relays connections back to its own inbound port, so callers can make their traffic appear to come
from the pod's own IP and get past network-based authorization」から確認。

## 事象

- `mesh::linkerd::conn::inbound` は元の宛先（SO_ORIGINAL_DST）と、相手が送る opaque transport の transport header の
  ポートをそのまま中継先にしていた。中継先がプロキシ自身のポート（inbound の 4143 等）でないことを確かめていなかった。
- Pod の IP の 4143 へ直接繋ぐと（4143 は iptables のリダイレクトの対象外）、平文・TLS の接続とも 4143 のポートの
  ポリシー（既定は all-unauthenticated）で認可され、アプリの代わりに `pod_ip:4143` へ中継された。入れ子の接続は送信元が
  Pod 自身の IP になるので、NetworkAuthentication（クラスタの Pod の CIDR 等）で守った別のポートの認可を、範囲の外の
  クライアントが通れた。transport header のポートに 4143 を書いても同じ。
- linkerd2-proxy は直接の接続（宛先が inbound のポート）を、ID のある mTLS で transport header を付けたものだけ受ける。

## 対応

- inbound の中継先をプロキシ自身のポート（inbound・outbound・管理・tap）にしない。宛先がプロキシのポートの接続
  （直接の接続）は、ID のある mTLS で transport header を付けたものだけ受け、平文・ID の無い接続・transport header の
  無い TLS は閉じる。transport header のポートは 0・範囲外（`i32` を `u16` へ切り詰めない）・プロキシのポートを断る。
- 利用者ガイド（`mesh.md` の英日）に書いた。

## テスト

- `mesh::linkerd::conn::tests::transport_target_port_refuses_proxy_ports`。
