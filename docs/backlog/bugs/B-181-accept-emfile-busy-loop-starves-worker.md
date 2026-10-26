# B-181: fd の上限（EMFILE）で accept が失敗し続けると、reactor のワーカーが空回りして接続が一切進まなくなる

**状態: 修正済み（v0.10.0）**

## 出典

F-276 の OpenBSD aarch64（`ssh llm` の VM）の `tests/api/library_e2e.sh upgrade` で、1000 本のアップグレード接続を同時に開く確認が
120 秒で打ち切られ、その後の確認（h2c の curl）も応答しなくなった（2026-10-26）。

## 事象

- OpenBSD の root の fd の上限は 1024（`ulimit -Hn`）。サーバは 1027 個の fd を持ち、1000 本が CLOSE_WAIT のまま残った（クライアントは
  とっくに閉じている）。ワーカースレッドが 40〜50% の CPU で回り続けていた。
- 原因: `listener_workers.rs`（と `l4/server.rs`）の accept のループは、accept の失敗を `error!` に出して **すぐに** やり直していた。
  fd の上限では、バックログに接続が残る限りリスナーは読み取り可能なので `Accept::poll` は毎回すぐに `Err(EMFILE)` を返し、reactor では
  ワーカーのタスクが Pending を返さずに回り続ける。同じワーカーの接続のタスク（閉じる処理を含む）が一切進まないので fd も空かず、
  固まったまま戻らない。ログも 1 周ごとに出る。io_uring は accept の完了を待つ間に他のタスクが進むので固まらないが、空回りとログは同じ。

## 修正

- `listener_workers::AcceptBackoff`: 資源不足（EMFILE・ENFILE・ENOBUFS・ENOMEM。Windows は WSAEMFILE・WSAENOBUFS）なら 100ms
  眠ってから受理し直す（nginx と同じ）。それ以外の失敗は 1 回だけ他のタスクへ譲る。資源不足のログは始まったときに 1 回だけ（受理に
  成功したら戻す）。HTTP のリスナー・L4 の 2 つのループで使う。`accept_batch` の資源不足はログを出さない（次の accept が扱う）。
- 要求ごとの経路は変わらない（accept が失敗したときだけ）。

## 試験

- `tests/standalone_e2e.rs` の `b181_accept_recovers_after_running_out_of_file_descriptors`: fd の上限を 128 にして起動し、200 本の
  接続を張って閉じた後、新しい要求が受理される（epoll の reactor で修正前は固まる）。
- `library_e2e.sh upgrade` の同時接続数を fd の上限に収める（OpenBSD の 1024）。
