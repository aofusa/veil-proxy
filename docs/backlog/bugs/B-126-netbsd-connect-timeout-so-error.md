# B-126: NetBSD で VDS・ACME の接続が拒否されても次のアドレスを試さなかった（std の connect_timeout が Ok を返す）

**状態: 完了（feat/cross-platform-api-vds-e2e。NetBSD での standalone E2E 中に発見）**

## 事象

NetBSD で `tests/standalone_e2e.rs` の 17 件中 9 件が `Connection reset by peer`・`Connection refused` で失敗した。
veil がまだ listen していないのに、試験の `wait_for_port` が「つながった」と判断して要求を送っていた。

## 原因

B-91 と同じ。NetBSD では、拒否された非ブロッキングの `connect` に対して `poll(2)` が `POLLOUT` だけを返し
（`POLLHUP`/`POLLERR` を立てない）、エラーは `SO_ERROR` に残る。Rust の `TcpStream::connect_timeout` は
`POLLHUP`/`POLLERR` しか見ないので `Ok` を返す（NetBSD 10.1 x86_64 で再確認）。

- 本体: 上流（ヘルスチェック等）は B-91 で `upstream::tcp_connect_timeout`（`take_error` を確かめる）にしていたが、
  制御プレーンの HTTP クライアント（`ctrl_http`。VDS・ACME）は std の `connect_timeout` をそのまま使っていた。
  名前解決で複数のアドレスが返るとき（`localhost` が `::1` と `127.0.0.1` など）、最初のアドレスが拒否されても
  つながったと見なして次を試さず、制御プレーンに接続できなかった。
- 試験: `tests/standalone/mod.rs` の `wait_for_port`・`http1_request`・`tls_peer_cert` も同じ。

## 修正

- `ctrl_http` の接続は `take_error` まで確かめる（`tcp_connect_timeout` は FreeBSD の接続ブローカーを経由するので使わない）。
- 試験の接続は `connect_local`（`take_error` まで確かめる）にまとめた。

## 試験

- NetBSD 10.1 x86_64 で standalone E2E 17 件が通過（F-203）。
