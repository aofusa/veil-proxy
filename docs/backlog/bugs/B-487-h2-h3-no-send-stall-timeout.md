# B-487: HTTP/2・HTTP/3 の応答の送信に時間切れが無い（読まないクライアントが接続と上流を握り続ける）

**状態: 未着手（2026-11-21 起票。機能 × プロトコルの網羅性の確認、feat/protocol-parity）**

## 出典

機能とプロトコルの対応の確認（`docs/artifacts/protocol-parity/audit.md` の #7）。

## 事象

- HTTP/1.1 は応答の書き込みを `WRITE_TIMEOUT`（30 秒）で包み、`buffering = "full"` は `client_write_timeout_secs`
  （既定 60 秒）で包む。読まないクライアントは時間が経てば切られる。
- HTTP/2 の送信（`Http2Connection::write_all_raw`）には時間切れが無い。クライアントが TCP の受信を止めると接続のループが
  書き込みで止まり、ストリームのタスク・中継中の上流の接続・溜めた応答を握ったまま、接続のアイドルの時間切れ（ストリームが
  あると効かない）にも掛からない。
- HTTP/2 の WINDOW_UPDATE・HTTP/3 の MAX_STREAM_DATA を送らずにストリームの送信を止める場合も、PING・ACK で接続を
  生かしておけば同じ（HTTP/3 は QUIC のアイドルの時間切れが受信のたびに延びる）。
- `[route.buffering] client_write_timeout_secs` の文書はプロトコルを限らない。

## 改修案

- HTTP/2 の接続の書き込みを `WRITE_TIMEOUT` で包む（満了したら接続を閉じる。HTTP/1.1 と同じ値）。
- 送る本文がフロー制御で止まったストリーム（HTTP/2・HTTP/3）は、送れない状態が `WRITE_TIMEOUT`（ルートに
  `client_write_timeout_secs` があればその値）続いたらリセットする（HTTP/2 は `RST_STREAM(CANCEL)`、HTTP/3 は
  `H3_REQUEST_CANCELLED`）。受信の止まりの検査（`h2_reset_stalled_streams`・`check_stalled_bodies`）と同じ 1 秒ごとの
  見回りで行い、要求ごとのタイマーを足さない。
- 性能: 書き込みの時間切れはソケットが詰まったときだけ費用が出る形にする。HTTP/2 の 3B・54KB を交互 A/B で確かめる。

## 受け入れ条件

- 統合: TCP の受信を止めた HTTP/2 のクライアントの接続が `WRITE_TIMEOUT` の後に閉じる。WINDOW_UPDATE を送らない HTTP/2 の
  ストリーム・MAX_STREAM_DATA を送らない HTTP/3 のストリームがリセットされ、上流の接続が返る。
- 文書（routing の `client_write_timeout_secs`・protocols、英日）。

## 関連

- [B-274](B-274-route-client-timeouts-h1-only.md)（受信の止まりの時間切れ）・[F-187](../features/F-187-graceful-shutdown-close-idle-keepalive.md)
