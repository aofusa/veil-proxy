# F-187: graceful shutdown でアイドルなキープアライブ接続をすぐ閉じる

**状態: 完了（feat/f186-f187-h3map-idle-shutdown）**

## 背景（B-115 / B-118 の確認中に判明）

graceful shutdown（`server::setup_signal_handler` → ワーカーの `drain_connections`）は、アクティブな接続数が
0 になるまで最大 `graceful_shutdown_timeout_secs`（既定 30 秒）待つ。HTTP の接続処理はシャットダウンを見て
いないため、要求を処理していないキープアライブ中の接続も「処理中」として数えられ、クライアントが 1 本でも
接続を保持していれば上限いっぱい待つ（実測: アイドルな HTTP/1.1 接続 1 本で 31.8 秒）。

- `docker stop`（猶予 10 秒）・Kubernetes（`terminationGracePeriodSeconds`）では猶予切れの SIGKILL になる。
- nginx は graceful shutdown でアイドルなキープアライブ接続を即座に閉じ、処理中の要求だけを待つ。

調べる途中で、HTTP/2 には接続のアイドルタイムアウトが無いこと（B-119）と、HTTP/3 の停止処理が GOAWAY を
送らず受信も止めていたこと（下記）も分かった。

## 改修

### ランタイム: アイドル待ちのタイムアウト

- `runtime::time::timeout_idle(duration, idle, future)`: `timeout` と同じだが、`idle = true` のタイマーには
  印が付く。`runtime::time::expire_idle_timeouts()` はそのスレッドの印付きタイマーだけを即座に満了させ、以後に
  始まるアイドル待ちも登録した時点で満了済みにする（io_uring・reactor の両方のタイマー）。
- ホットパスへの追加はタイマーのスロットと `Sleep` の bool 1 個と、登録時の分岐 1 個だけ（要求ごとの
  登録・解除や、フラグの監視は足さない）。停止時の満了は O(スロット数) の走査を 1 回。
- `drain_connections`（各ワーカーが accept ループを抜けたあと）の先頭で `expire_idle_timeouts()` を呼ぶ。
  接続はそれを受け付けたワーカーのスレッドで動くので、各ワーカーが自分の接続を閉じさせる。

### HTTP/1.1

`handle_requests` の次の要求待ち（蓄積が空のとき）を `timeout_idle` にした。要求の途中まで届いている接続は
待つ。処理中の要求は応答を返して次の待ちに入った時点で閉じる（パイプライン化されて既に届いている次の要求は
処理する）。

### HTTP/2（B-119 を含む）

処理中のストリームも分割フレームの続きも無いときの待ちを `timeout_idle(IDLE_TIMEOUT = 60 秒)` にした。満了
（アイドルタイムアウト・停止）したら `GOAWAY(NO_ERROR, last_stream_id = 受け付けた最大の ID)` を送り、送信側を
閉じて上限付きで読み捨ててから閉じる（`Http2Connection::close_with_goaway`。不正なプリフェースの拒否と共通。
未読データで RST にならないように。B-102）。処理中のストリームがある間はタイマーを張らない（従来どおり）。

### HTTP/3

停止処理は「ログに GOAWAY を送ると出すだけで実際には何も送らず、受信も送信も止めて quiche の
`on_timeout` だけを回す」もので、処理中の要求は進まず、接続は quiche のアイドルタイムアウト（30 秒）で
消えるのを待っていた（上限も設定値ではなく 30 秒の固定値）。

- 停止中もメインループ（受信・バックエンド・送出）を回し続ける。
- 各接続に GOAWAY（ID = 受け付けた最大の要求ストリーム ID + 4）を送る。以後に始まった要求は
  `H3_REQUEST_REJECTED` で断る（quiche は GOAWAY 後の要求を拒否しないので veil 側で判定する）。
- 処理中の要求（応答の送出途中・バックエンド待ち・本文の受信途中）が無くなった接続から
  `CONNECTION_CLOSE(H3_NO_ERROR)` を出す。全接続が閉じるか `graceful_shutdown_timeout_secs` で抜ける。

## 確認

- 統合テスト（`b115_shutdown_signals`）: キープアライブ中の HTTP/1.1 接続を残した SIGTERM で 8 秒以内に
  終了し、`Drain timeout` が出ずに接続が閉じる / 要求の無い h2c 接続が `GOAWAY(NO_ERROR)` を受けて閉じる。
  修正前はどちらも 30 秒待つ。
- 手動（遅い上流: 応答まで 3 秒）で、要求の処理中に SIGTERM:
  HTTP/1.1・h2c・HTTP/2 は応答 200 を受け取ってから 2.1 秒で終了。HTTP/3（h2load、2 接続 × 2 並列）は
  処理中の 4 要求がすべて成功し、2.1 秒で終了。1 接続 4 要求にすると後半の 4 要求は GOAWAY を受けて始まらない。
- E2E（io_uring・epoll とも 565 件成功）の後片付けで、修正前は veil の一部が SIGTERM の 5 秒後に
  `Force killing process` で SIGKILL されていた（テストクライアントのキープアライブ接続を待っていた）。
  修正後は全プロセスが SIGTERM だけで終了する。
