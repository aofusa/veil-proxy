# B-378: `[security] blocked_ips` が HTTP/3 と L4 のリスナーに掛かっていなかった

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx の 2 回目のセキュリティスキャン（`CLAUDE-SECURITY-20261114-024333`）の候補から確認。

## 事象

- 全体の IP ブロックリスト（`[security] blocked_ips`。F-35）は HTTP の TCP のリスナーの accept の直後
  （`listener_workers`）だけで見ていた。文書は「accept の直後に切断する」としか書いておらず、利用者は全部の
  リスナーに掛かると読む。
- HTTP/3（UDP）は新しい接続の Initial で見ておらず、ブロックした送信元も HTTP/3 なら全部のルートへ届いた
  （ルート単位の `denied_ips` は掛かる）。L4 の TCP・UDP のリスナー（`[[l4]]`）は IP の制限を何も見ていなかった。

## 対応

- HTTP/3: 知らない接続 ID の Initial を受けたとき、送信元がブロックリストにあれば捨てる（Retry も返さない）。
  確立済みの接続のパケットには掛けない（TCP でもリロードで確立済みの接続は切らない）。
- L4 TCP: accept の直後に断る（データプレーンの L4 と、`l4-handler` のライブラリの L4 の両方）。
- L4 UDP: 新しいセッションを作らない。
- いずれも新しい接続のときだけ見るので、要求・パケットごとの費用は増えない。

## テスト

- `standalone_e2e::blocked_ips_apply_to_http3_and_l4`（127.0.0.1 を外した設定では HTTP/3 と L4 が通り、
  入れた設定では通らない）。
- 文書: `docs/guide/routing.md`（英日）の `blocked_ips`、`examples/config.toml`。
