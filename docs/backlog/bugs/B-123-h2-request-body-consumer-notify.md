# B-123: HTTP/2 の要求本文を受け取る側がメインループを起こさず、1MiB を超える本文で応答が返らなかった（reactor）

**状態: 完了（feat/cross-platform-api-vds-e2e。各 OS での API の E2E 中に macOS で発見）**

## 事象

macOS（kqueue）で `tests/api/library_e2e.sh grpc` の `h2 large_unary (3MiB)` が 2 回に 1 回固まり、300 秒のタイムアウトで
失敗した。Linux の epoll（`--features epoll`）では 8 回中 8 回固まる。io_uring（Linux の既定）では再現しない。
固まっている間、サーバは 1,049,023 バイト（接続の初期受信ウィンドウ 1MiB ぶん）を受け取ったところで止まり、
WINDOW_UPDATE を 1 つも送っていなかった（ソケットの送受信キューはどちらも空）。

## 原因

HTTP/2 の多重化（F-116）では、要求本文はメインループが `req_pending` からストリームごとのチャネルへ移し、移せたぶんだけ
recv ウィンドウを補充する（消費連動のバックプレッシャ）。チャネルを読む側（ストリームのタスク）は、読んだら
`Notify::notify` でメインループを起こす約束になっている（起こさないとメインループはソケットの読み取り待ちのままで、
クライアントはウィンドウが無いので何も送らない）。次の箇所がこれを守っていなかった。

- API ライブラリのハンドラへ本文を流すポンプ（`proxy::h2_api_handler`、F-197）。gRPC のハンドラ・`body_stream` を使う
  ハンドラへの 1MiB を超える要求が固まる。
- 要求本文を読み捨てる 7 か所（上流に接続できない・上流が途中で閉じた・ハンドラが本文を捨てた等で 502 などを返す前）。
  1MiB を超える本文の要求は、エラー応答が返らずに固まる。

io_uring で再現しなかったのは、メインループが他の契機で起きて先へ進んでいたため（reactor では起床の契機が他に無い）。

## 修正

- 読み捨てを `h2_drain_req`（消費のたびに `notify`）にまとめ、7 か所すべてで使う。
- ハンドラへのポンプも 1 断片ごとに `notify` する（プロキシのアップロード経路と同じ）。

## 試験

- `tests/standalone_e2e.rs` の `b123_h2c_large_body_to_failing_upstream_gets_502`: 接続できない上流へのプロキシに 3MiB の
  h2c POST を 2 回送り、どちらも 502 が返る。epoll ビルドで修正前のコードは 20 秒のタイムアウトで失敗することを確認。
- `tests/api/library_e2e.sh grpc`: Linux では io_uring に加えて epoll のビルド（`--features veil/epoll`）でも gRPC の
  相互運用試験を回す。修正後は epoll で 5 回中 5 回・macOS で通過。
