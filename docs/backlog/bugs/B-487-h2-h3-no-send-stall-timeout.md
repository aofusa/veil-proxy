# B-487: HTTP/2・HTTP/3 の応答の送信に時間切れが無い（読まないクライアントが接続と上流を握り続ける）

**状態: 修正済み（feat/protocol-parity。2026-11-21 起票・2026-11-22 修正。機能 × プロトコルの網羅性の確認）**

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

## 対応（2026-11-22）

- 接続（HTTP/2）: 書き込み（`Http2Connection::write_all_raw`・FreeBSD の `flush_zero_copy`）を接続に 1 つの見張りのタイマー
  （`watch_write`）で包んだ。書き込みが初めて Pending になったときに `WRITE_TIMEOUT` 後のタイマーを張り、満了の時点で前回から
  書き込みが 1 つも終わっていなければ接続を閉じる（`Http2Error::Io(TimedOut)`。呼び出し側は `debug!`）。終わっていれば
  張り直す。書き込みごとのタイマーは張らないので、閉じるのは止まってから 30〜60 秒。reactor ですぐ終わる書き込みは費用が無い
  （io_uring は Pending の書き込みごとに張ってあるタイマーへ Waker を置き直すだけ）。
- ストリーム（HTTP/2・HTTP/3）: 応答の続きをフロー制御で送れないストリーム（HTTP/2 は `pending_body`、HTTP/3 は保留中の
  見出し・本文・終端・トレーラと直接の応答 `PartialResponse`）を、受信の止まりの見回り（1 秒ごと。送れないストリームがある
  間だけ張る）で数える。送った量（`l7::SendStallWatch`）が `WRITE_TIMEOUT` を超えて増えなければ `RST_STREAM(CANCEL)`・
  `H3_REQUEST_CANCELLED` で打ち切る。`buffering = "full"` の応答は `client_write_timeout_secs`（HTTP/2 は head の前の
  `H2RespMsg::SendTimeout`、HTTP/3 は `classify` → `BufferedReq`）。ストリームの表から外すので、タスクは上流の接続を放す。
- 自分のウィンドウは残っていて接続のウィンドウ（HTTP/3 は MAX_DATA と輻輳窓）を待つストリームは、接続がほかのストリームで
  進んでいれば止まっていないとみなす。空いたクレジットを veil の順番で先のストリームが使い続けるので、ストリームごとの進み
  だけを見ると、遅い回線で並行に取るクライアントの後のストリームを打ち切る。HTTP/2 はストリームの送信ウィンドウ、HTTP/3 は
  quiche の `writable`（空なら接続のクレジットが無い）と `stream_capacity` で見分ける。
- WebSocket のトンネル（拡張 CONNECT）も見張る。HTTP/1.1 の中継は書き込みを `WRITE_TIMEOUT` で包む（B-306）が、HTTP/2・
  HTTP/3 の中継は応答のチャネルの空きを待つので、`websocket_idle_timeout_secs = 0`（既定）だといつまでも止まっていた。
- 試験: 単体 `send_stall_watch_counts_ticks_without_progress`。自己完結型 E2E（`tests/standalone/protocol_parity_tests.rs`）:
  - `b487_buffered_response_stalled_by_flow_control_is_reset`: h2c・HTTP/2・HTTP/3。`client_write_timeout_secs = 2` の
    ルートは 8 秒止めるまでにリセット、既定のルートは 8 秒止めても最後まで届く。
  - `b487_stalled_clients_release_upstream_after_write_timeout`: TCP を読まない h2c の接続が 30〜60 秒で閉じる。WINDOW_UPDATE を
    送らないストリーム・WebSocket のトンネル・MAX_STREAM_DATA を出さない HTTP/3 のストリーム（PING で QUIC のアイドルを
    避ける）が 30 秒余りでリセットされる。どれも上流の接続を放す。
  - `b487_slow_reader_of_parallel_streams_is_not_reset`: 接続のウィンドウだけが小さい遅い読み手の 3 本の並行の取得
    （h2c・HTTP/3）を打ち切らない。
  - 修正前のソースでは前の 2 つが失敗し（応答が最後まで届く・上流を 80 秒握る）、接続の進みを見ない版では 3 つ目が失敗する
    （2 本目のストリームを `CANCEL`）ことを確かめた。
- 性能: 交互 A/B（h2c の静的配信。veil 2 スレッド・CPU 0,1、veil-load `--mode h2 --streams 10 -c 64`・CPU 2,3。6 回の
  中央値）で 3B は rps −0.9%・CPU/要求 +1.2%、54KB は rps +1.4%・CPU/要求 −2.3%（揺れの範囲。同じ機械で別の作業の試験が
  走っていた）。io_uring で待たされた書き込みに足すのは見張りの `Sleep::poll`（スレッドローカルの借用 2 回。タスクの Waker の
  clone はビットコピー）だけで、タイマーを張るのは接続で最初の 1 回と満了の後だけ。
- 文書: protocols・performance・compression-cache・migration-nginx（`send_timeout` の行。英日）、`examples/config.toml`。

## 関連

- [B-274](B-274-route-client-timeouts-h1-only.md)（受信の止まりの時間切れ）・[F-187](../features/F-187-graceful-shutdown-close-idle-keepalive.md)
