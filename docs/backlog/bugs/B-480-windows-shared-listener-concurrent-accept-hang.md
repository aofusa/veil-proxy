# B-480: Windows で同じ待ち受けのソケットを共有するワーカーが同時に accept し、片方が accept の中で止まって受理済みの接続に応答しなくなる

**状態: 修正済み（fix/security-scan-fable-max、2026-11-19）**

## 出典

[B-460](B-460-listen-sockets-reuseaddr-cobind.md) を main（e34dc2c0）へ入れた後の `bindings` の Windows のジョブが、
2 回続けて応答の無い要求で落ちた。1 回目は `library_e2e.sh python` の WebSocket の同時 2,000 接続
（`ws_check.py many`）で 57 分止まって取り消し、再実行では `crates/veil-python/tests/test_lane.py` の
`test_all_threads_blocked_then_the_queue_moves_again` が 30 秒の時間切れになった。B-460 より前の main の Windows の
`bindings` は通っていた（cdd9922f では 1 回通っている。確率で起きる）。

## 事象

- B-460 で Windows の `TcpListener::bind_reuse_port` は、同じアドレスの全ワーカーで 1 本の待ち受けのソケットを共有し、
  各ワーカーは `WSADuplicateSocketW` の複製で readiness を待って `accept` する。接続が来ると全ワーカーの WSAPoll が
  起き、同じソケットで同時に `accept` を呼ぶ。
- mswsock の非ブロッキングの `accept`（`WSPAccept`）は、接続が並んでいるかを確かめてから、待つ形の要求を AFD へ出す。
  その間に別のワーカーが同じ接続を受理すると、要求は次の接続が来るまで返らない。止まったワーカーは、先に受理して
  タスクにした接続（同じ周回の `accept_batch` で前に受理した分を含む）も進めないので、その要求には応答しない。
  受理の待ちの 1 秒の区切り（停止の確認）も、スレッドが `accept` の中にいるので効かない。
- aofish（Windows 11）で、test_lane のサーバ（`threads=2`）を起動しては要求 8 本（`/sleep/200` 3 本・`/quick` 5 本）を
  同時に送るのを繰り返すと、26 回中 6 回で要求が時間切れになった。止まった時点のスタックを cdb で取ると、ワーカーの 1 本が
  `Accept::poll` → `raw_accept_one` → `mswsock!WSPAccept` → `SockWaitForSingleObject` で待っていた（もう 1 本は
  WSAPoll、Python の実行線は空き。2 回とも同じ）。
- B-460 の前は各ワーカーが自分のソケット（`SO_REUSEADDR`）で受理していたので、同じソケットで 2 つのスレッドが同時に
  `accept` することは無かった。

## 対応

- 共有の待ち受けのソケット（`SharedListenSocket`）に受理の番（`AtomicBool`）を持たせ、`raw_accept_one`
  （`Accept::poll` と `accept_batch` の共通の受理）は番を取ってから `accept` を呼ぶ。番を取れなかったワーカーは
  `WSAEWOULDBLOCK` と同じ扱いで readiness を待ち直す（接続が残っていれば WSAPoll がすぐに起こす。待たないので
  ワーカーは止まらない）。プロセスの中で同時に `accept` を呼ぶのは 1 つだけになり、mswsock の確認と待ちの間に
  接続が他のワーカーへ取られることが無くなる。受理ごとに比較交換 1 回と書き込み 1 回だけ足す。
- 共有しないリスナー（`bind`・ポート 0）は従来どおり番を取らない。

## テスト

- Windows の単体試験 `runtime::reactor::tcp::windows::tests::concurrent_accepts_on_a_shared_socket_do_not_block`
  （4 スレッドが複製で readiness を待ってまとめて受理し、ワーカーと同じく次の受理を試みてから、受理した接続へ 1 バイト
  返す。同時に張る 8 接続を 100 回。受理の中で止まったスレッドが先に受理した接続は応答されずに時間切れになる）。
  aofish で修正前は 6 回中 6 回落ちた（受理した接続に応答しない）。受理した直後に応える形・接続を 1 本ずつ張る形の
  試験では修正前でも通った（止まっても受理済みの接続が残らない・同時に並ばない）。修正後は 6 回とも通る。
- aofish の再現（上の test_lane のサーバの起動と同時 8 要求）: 修正前 26 回中 6 回で時間切れ → 修正後 200 回中 0 回。
  `ws_check.py python` と `many 2000`（examples/api/python-websocket）は修正後 4 回とも通る。`test_lane.py` を続けて
  回すと、修正前は 14 回中 4 回で要求が 30 秒の時間切れ → 修正後は 7 回とも通る（どちらもその後は試験の繰り返しで
  一時ポートが尽きて `WinError 10048` で落ちた。時間切れではない）。
- `SO_EXCLUSIVEADDRUSE` の待ち受けは、そのポートにサーバ側の TIME_WAIT や受理済みの接続が残っていても同じポートへ
  bind し直せる（aofish で確かめた。B-460 で再起動が遅れることは無い）。
- Windows の単体 1,206（`full-windows`）・`cargo clippy --lib --bins --tests --features full-windows -- -D warnings`。
- `bindings` の CI（Windows の `library_e2e.sh`）。

## 関連

- [B-460](B-460-listen-sockets-reuseaddr-cobind.md)（待ち受けのソケットの共有）、
  [F-155](../features/F-155-freebsd-perf-nginx-parity.md)（`accept_batch`）、[F-125](../features/F-125-windows-macos.md)
  （Windows の reactor）。
