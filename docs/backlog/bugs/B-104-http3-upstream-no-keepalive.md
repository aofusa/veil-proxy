# B-104: HTTP/3 の上流接続がプールされない（1 要求 1 TCP 接続）

**状態: 対応中（feat/v080-limitations）**

## 事象

HTTP/3 で受けた要求の上流接続は、どの経路も `Connection: close` 付きで 1 要求ごとに新規 TCP 接続を張る。

- ストリーミング経路（`http3_stream::backend_task`）: `TcpStream::connect_str` →（HTTPS なら）`tls_connect`。
  head は `build_h1_request_head` が `Connection: close` 付きで作る（`http3_server.rs`）。
- バッファ経路の平文上流（`proxy_to_backend_async_with_tls`）も `Connection: close`。

HTTP/1.1・HTTP/2 で受けた要求は `HTTP_POOL` / `HTTPS_POOL`（B-93 の生存確認込み）を使うのに、HTTP/3 だけ使っていない。

## 影響

- 要求ごとに connect / close と TIME_WAIT が発生する。高負荷では一時ポートを消費する（B-42 の有力な原因）。
- HTTPS 上流は要求ごとにフル TLS ハンドシェイク（上流側の CPU も消費）。

## 改修

- head から `Connection: close` を外し、応答の終端（`Content-Length`・chunked の終端）が確定し、本文を送り切った場合だけ
  接続をワーカーのプール（`HTTP_POOL` / `HTTPS_POOL`）へ返す。EOF 終端・エラー・リセット・早期応答では返さない。
- 取り出しは既存の `pooled_conn_reusable`（B-93 の規則）を使う。

## 見積もり

平文上流 +10〜30%、HTTPS 上流 2〜5 倍（クライアント律速でない計測系で）。
