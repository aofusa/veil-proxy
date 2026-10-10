# B-104: HTTP/3 の上流接続がプールされない（1 要求 1 TCP 接続）

**状態: 完了（feat/v080-limitations）**

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

## 結果（2026-10-09）

- HTTP/3 ワーカーにスレッドローカルの上流接続プール（`H3_BACKEND_POOL`、平文・TLS 共通の `BackendIo`）を追加し、
  ストリーミング経路（`backend_task`）・バッファ経路（`exchange_buffered`）とも再利用する。`Connection: close` は付けない。
- 応答の終端をちょうど読み切った（Content-Length 一致・chunked の終端・HEAD/204/304）接続だけを戻す。EOF 終端・余剰データ・
  クライアント切断・101 は戻さない。TLS は復号済み平文が残っていれば戻さない。
- 取り出し時は B-93 と同じく、アイドル 1ms 以上なら `MSG_PEEK` で生存確認する。
- F-177: 再利用した接続が応答の前に失敗し、本文の無い冪等メソッドなら新規接続で 1 回だけ再送する。
- E2E: `test_b104_http3_reuses_upstream_connection`（平文・TLS とも 6 要求で上流接続 ≤ 3 本。並行テストと上流を共有するため
  1 本固定にはしない）、`test_b104_http3_head_on_keepalive_upstream`。
