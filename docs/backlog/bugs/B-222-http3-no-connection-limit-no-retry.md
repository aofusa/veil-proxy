# B-222: HTTP/3 に接続数の上限と Retry が無く、送信元を偽装した Initial の洪水でワーカーのメモリを使い切れる

**状態: 修正済み（v0.10.0）**

## 出典

2026-10-27 のセキュリティレビュー（v0.9.2 のコード読解）。

## 事象

- `http3_server/event_loop.rs::process_datagram_segments` は、未知の接続 ID の Initial（1200 バイト以上）が届くたびに
  `quiche::accept_with_buf_factory` を**無条件に**呼び、quiche の接続（TLS の状態・受信バッファ）を確保して `max_idle_timeout` まで保持する。
- `http3_server/mod.rs`（L453〜L465 付近）に `quiche::retry`（ステートレスな Retry によるアドレス検証）が無い。
- ワーカーあたりの接続数・ハンドシェイク中の接続数の上限が無い。quiche の 3 倍の増幅制限は帯域しか守らない。
- 送信元 IP を偽装した UDP の洪水で、`max_idle_timeout` 秒のあいだ受信レートに比例してメモリが増える。

接続 ID の HMAC 導出（B-94）は「同じクライアントの再送で別接続を作らない」対策で、偽装された別の送信元には効かない。

## 再現手順（案）

1. `[server].listen_http3` を有効にする。
2. 送信元 IP をランダムにした QUIC の Initial（1200 バイトに詰める）を 1 秒あたり数万通送る。
3. HTTP/3 ワーカーの RSS が `max_idle_timeout` 秒のあいだ受信レートに比例して増えることを確かめる。

## 改修案

- ワーカーごとの接続数の上限（`[http3] max_connections`、既定は例えば 10000）と、ハンドシェイク中の接続数の上限。
- ハンドシェイク中の接続数が閾値を超えたら `quiche::retry` でアドレス検証を要求する（トークンは B-94 の HMAC 鍵で作る）。
- ハンドシェイク専用の短いタイムアウト（確立前の接続は `max_idle_timeout` より早く捨てる）。
- 設定キーを足すので `examples/config.toml`・`docs/guide/`（英日）を同じ変更で更新する。
- メインループはダーティ集合でイベント駆動（B-12/F-151）のまま。接続数の数え方に全接続のスイープを足さない。

## 受け入れ条件

- 偽装 Initial の洪水の負荷試験（`tools/container_security/chaos` の `l4_flood_probe` の UDP 版。F-279）で RSS が上限で頭打ちになる。
- 正常なクライアント（quinn の E2E・h2load の HTTP/3）が Retry を越えて接続できる。HTTP/3 の性能の交互 A/B が変わらない。

## 修正（v0.10.0）

- 新しいモジュール `http3_server::admission` で新しい接続の受け入れを制御する（状態はワーカーのスレッドローカル。ハンドシェイク中の数は接続の生成・確立（`init_h3`）・破棄で増減し、全接続を数え直さない）。
  - `[http3] max_connections`（ワーカーごと、既定 10000、0 は上限なし）: 達している間は新しい接続の Initial を捨てる。
  - `[http3] retry_threshold`（既定 1000、0 は常に）: ハンドシェイク中の接続がこの数に達したら、トークンの無い Initial に `quiche::retry` でステートレスな Retry を返す。トークンは期限（10 秒）・元の DCID・送信元のアドレスとポートに対する HMAC-SHA256（プロセス共有の鍵、先頭 16 バイト）で、定数時間で検証する。トークン付きの Initial は検証を通ったときだけ `accept(scid = Retry で渡した接続 ID, odcid)` で接続を作る。Retry は受信の処理で溜め、`send_pending_packets` が送信の束の先頭に積む。
  - `[http3] handshake_timeout`（既定 10000ms）: 確立しない接続はこの期限で捨てる（タイマーの期限にハンドシェイクの期限も含める）。
- 対応しない版の Initial は accept せずに捨て、`accept` の失敗はワーカーを落とさず（以前は `?` でメインループから抜けていた）その Initial だけ捨てる。
- 試験: `test_b222_retry_then_handshake_and_connection_limit`（quiche のクライアントで Retry → トークン付きの Initial → ハンドシェイク完了、別の送信元のトークンの拒否、接続数の上限）、`admission::tests`（トークンの改ざん・期限・送信元違い、判定と数え方）。
- 設定のキーは `examples/config.toml`・`docs/guide/configuration.md`（英日）・`security.md`（英日）に追記した。

## 関連

- [B-94](B-94-http3-retransmitted-initial-new-connection.md)、[B-147](B-147-h3-buffered-request-body-unbounded.md)、[F-279](../features/F-279-security-test-gaps.md)
