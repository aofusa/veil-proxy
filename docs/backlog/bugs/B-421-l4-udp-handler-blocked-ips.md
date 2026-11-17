# B-421: ライブラリの L4 の UDP のハンドラが `[security] blocked_ips` を当てずにデータグラムを渡し、応答も返していた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-17）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「`[security] blocked_ips` is not applied to l4-handler UDP listeners」・「Native L4 UDP handlers skip the global
[security] blocked_ips deny list」（同じ指摘が 2 件）から確認。

## 事象

- `api::l4::serve_udp`（`l4-handler` の UDP のハンドラ）は、`recv_from` で受けたデータグラムをすべてハンドラへ渡し、
  応答を送り元へ返していた。B-378 で全部のリスナーに当てた `config::is_ip_blocked` を呼んでおらず、TCP のハンドラ
  （`l4::server`）・L4 の UDP の中継（`l4::udp`）と違って、拒否リストの送り元もハンドラに届いて応答を受け取れた。

## 対応

- データグラムごとに `config::is_ip_blocked` を見て、拒否リストの送り元は捨てる。
- 利用者ガイド（`routing.md` の英日）と `examples/config.toml` の `blocked_ips` の説明に書いた。

## テスト

- 判定は既存の `config::is_ip_blocked` の単体テスト。`l4-handler` のライブラリの E2E（`tests/api/library_e2e.sh`）で
  UDP のハンドラが従来どおり応答することを確かめる。
