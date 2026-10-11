# F-187: graceful shutdown でアイドルなキープアライブ接続をすぐ閉じる

**状態: 未着手**

## 背景（B-115 / B-118 の確認中に判明）

graceful shutdown（`server::setup_signal_handler` → ワーカーの `drain_connections`）は、アクティブな接続数が
0 になるまで最大 `graceful_shutdown_timeout_secs`（既定 30 秒）待つ。HTTP の接続処理はシャットダウンを見て
いないため、要求を処理していないキープアライブ中の接続も「処理中」として数えられ、クライアントが 1 本でも
接続を保持していれば上限いっぱい待つ（実測: アイドルな HTTP/1.1 接続 1 本で 31.8 秒）。

- `docker stop`（猶予 10 秒）・Kubernetes（`terminationGracePeriodSeconds`）では猶予切れの SIGKILL になる。
- nginx は graceful shutdown でアイドルなキープアライブ接続を即座に閉じ、処理中の要求だけを待つ。

## 案

- HTTP/1.1: 要求の切れ目（次の要求の待ち）で `SHUTDOWN_FLAG` を見て閉じる。処理中の要求は応答に
  `Connection: close` を付けて終える。
- HTTP/2: GOAWAY（last-stream-id = 処理中の最大）を送り、処理中のストリームが終わったら閉じる。
- HTTP/3: GOAWAY と CONNECTION_CLOSE。
- キープアライブの待ちはホットパスなので、フラグの確認は待ちの起床時（タイムアウト・readiness）だけにする。
