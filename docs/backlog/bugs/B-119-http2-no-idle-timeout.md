# B-119: HTTP/2 の接続にアイドルタイムアウトが無く、要求の無い接続を無期限に保持する

**状態: 完了（feat/f186-f187-h3map-idle-shutdown）**

## 事象

HTTP/2（TLS・h2c）の接続は、処理中のストリームが無くても無期限に残る。HTTP/1.1 は次の要求を
`IDLE_TIMEOUT`（60 秒）まで待って閉じるが、HTTP/2 のメインループ（`proxy::handle_http2_requests`）の待ちは
「ソケットが読める / ストリームのタスクからの通知」だけでタイマーを持たない。ストリーム単位のアイドル
（`stream_idle_timeout_secs`、Slow Loris 対策）はあるが、ストリームが 1 本も無い接続には効かない。

クライアントが接続を開いたまま放置するだけで、fd と接続ごとのバッファ（読み込みバッファ・HPACK の表など）を
握り続けられる。graceful shutdown もこの接続を上限まで待っていた（F-187）。

## 改修

F-187 と同じ仕組み（`runtime::time::timeout_idle`）で、処理中のストリームも分割フレームの続きも無いときの
待ちに `IDLE_TIMEOUT`（60 秒、HTTP/1.1 と同じ）を付けた。満了したら `GOAWAY(NO_ERROR)` を送り、送信側を
閉じて上限付きで読み捨ててから閉じる。処理中のストリームがある間はタイマーを張らない。

確認は統合テスト `sigterm_sends_goaway_to_idle_h2c`（停止時に同じ経路で GOAWAY を受ける）。
