# F-205: HTTP/1.1 のキープアライブ待ちを 1 回の操作にする（io_uring の POLL_ADD → RECV・epoll の確認用 poll）

**状態: 未着手（設計済み 2026-10-14）**

## 背景（F-202 の計測）

HTTP/1.1 の接続ループ（`proxy::handle_requests`）は、応答を返したあと次の要求を readiness 待ち
（`ServerTls::wait_next_request`）→ 読み取りバッファを借りて読む、の 2 段で待つ（B-101: アイドル接続に 64KB の
読み取りバッファを持たせない）。このため 1 要求あたり、io_uring は `POLL_ADD` と `RECV` の 2 操作（完了の往復が 1 回多い）、
epoll は確認用の `poll(2)`・`read(2)`・`sendto(2)` の 3 syscall になる。

F-202 の後、API ライブラリのネイティブハンドラは tokio 上の axum に対して epoll 98%・io_uring 93%。残りの差の大半はこの
2 段の待ちと見ている（axum + `veil::tokio` は epoll で `recvfrom` 0.9・`writev` 0.5・`poll` 0.5・`epoll_ctl` 0.5）。
プロキシの HTTP/1.1 も同じ経路を通るので、プロキシにも効く。

## 現状のコード（2026-10-14）

- `handle_requests`（`src/proxy.rs`）: `wait_first = !first_read && accumulated.is_empty()` のとき
  `timeout_idle(IDLE_TIMEOUT, idle, async { wait_next_request().await?; read(buf_get()).await })`。
  読んだ `SafeReadBuffer`（64KB、`pool::buf_get`）を `accumulated`（`accum_buf_get`）へ `extend_from_slice` で写してから返す。
- `wait_next_request`（`src/simple_tls.rs`・`src/ktls_rustls.rs`）: rustls が未処理の平文を持っていれば即 Ready、
  そうでなければ `readable()`（io_uring は `POLL_ADD`、epoll は `EPOLL_CTL_MOD` + 確認用 `poll(2)`、kqueue は `readable_lazy`）。
- io_uring の provided buffer ring（`IORING_REGISTER_PBUF_RING`）は HTTP/3 の UDP 受信（F-130 C2、`runtime::uring::udp_recv`）で
  使っており、register 操作は io_uring の制限（`apply_restrictions`）・seccomp で既に許可されている。`RECV` も許可リストにある。
  **新しいオペコード・syscall は要らない。**

## 設計

### io_uring: provided buffer ring の `RECV` 1 回で待つ

1. ワーカーごとに TCP 受信用のリングを 1 個登録する（`runtime::uring::tcp_recv_ring`、新規）。
   - バッファ群 ID は HTTP/3 のリングと別にする。既定 128 本 × 4KB = 512KB / ワーカー。リングのバッファを持つのは
     「完了してから `accumulated` へ写すまで」の一瞬だけなので、本数は 1 回の刈り取りで同時に完了する数を賄えればよい。
   - `[performance] keepalive_recv_buffers`・`keepalive_recv_buffer_size` で変えられるようにし、`0` で無効（今の 2 段）。
     `examples/config.toml` と `docs/guide/`（英日）に載せる。
   - 登録に失敗（カーネル 5.19 未満・制限）したら今の 2 段で動く（起動時に 1 回 debug ログ）。
2. `handle_requests` の待ちで、平文（`ServerTls` が平文のもの）のときは `wait_next_request` + `read` の代わりに
   `RECV`（`IOSQE_BUFFER_SELECT`、バッファ群 ID）を 1 回出す。
   - 完了したら CQE の `IORING_CQE_F_BUFFER` からバッファ ID を取り、`accumulated` へ写してすぐリングへ返す
     （写すのは今の 64KB バッファからの写しと同じ 1 回。写しの回数は増えない）。
   - 4KB を超える要求は、続きを今の経路（蓄積が空でないので `buf_get` の 64KB で読む）で読む。
   - `-ENOBUFS`（リングが空）のときは、その回だけ今の 2 段で待つ。
3. 取り消し: アイドルタイムアウト・graceful shutdown（F-187 の `expire_idle_timeouts`）で待ちを打ち切ると `RECV` を
   キャンセルする。キャンセルと完了が行き違ってバッファ付きの CQE が後から来たら、op 表の完了処理でバッファをリングへ返す
   （接続は閉じるのでデータは捨ててよい。リングのバッファが漏れないことだけ保証する）。F-116 の「`ReadFuture` を drop すると
   データを失う」問題は、この待ちでは「drop = 接続を閉じる」なので起きない。HTTP/2 の待ちには使わない。
4. TLS:
   - kTLS の受信（`KtlsFull`）は制御レコードを `recvmsg` の cmsg で見分ける必要があるので対象外（今の 2 段）。
   - rustls（ユーザ空間の TLS）は第 2 段階: 暗号文をリングのバッファで受けて `read_tls(&mut &buf[..n])` に渡す。
     B-86（`read_tls` ごとに平文を排出）を守る。効果を計測してから入れるかを決める。

### epoll: 先に読んでみる（B-75 を守る形で）

- 応答を返した直後に、ノンブロッキングの `recv` を先に 1 回試す（バッファはスレッドローカルのプールから借り、
  `EAGAIN` ならすぐ返す。アイドル接続はバッファを持たないまま）。
  - データが返れば、待ち（`EPOLL_CTL_MOD` + 確認用 `poll(2)`）を丸ごと省ける。`recv` の結果そのものが真の readiness なので、
    B-75 で外した「イベントを見たというヒント」で Ready を返す最適化とは違う（待ちを省くのは実際に読めたときだけで、
    待つときは必ず `register()` で再武装する）。
  - `EAGAIN` なら今の待ちに入る（syscall が 1 本増える）。
- 当たり外れは負荷で決まる（クライアントが応答を受けてから次を送るまでの間にワーカーが戻ってくるか）。接続ごとに直近の
  結果を 2 ビットの飽和カウンタで覚え、外れが続く接続では先読みをしない（`EAGAIN` の空打ちを抑える）。
- kqueue は `readable_lazy`（F-141）が既に読み取り可能バイト数のヒントで確認を省いているので変えない。
  WSAPoll（Windows）も変えない。

### 付随: 蓄積バッファへの写し

io_uring の 1 回目の読み取り（リング）以外の読み取りも、読み取りバッファ → `accumulated` の写しが毎回ある。
`accumulated` の空き領域に直接読む形にできるかを同じチケットで確かめる（効果が無ければ見送る）。

## 試験

- 単体（`runtime::uring`）: リングの登録・バッファ ID の往復・`ENOBUFS` からの戻り・キャンセルと完了の行き違いで
  バッファが返ること・リング無効時の経路。
- E2E: `./tests/e2e_setup.sh test` と `VEIL_E2E_FEATURES="full,epoll"`（reactor に触れるので必須）。
  キープアライブの試験（パイプライン・長い要求・アイドルタイムアウト・graceful shutdown）が既にあることを確認し、
  4KB を超えるヘッダの要求を足す。
- `library_e2e.sh` 全節、`tests/standalone_e2e.rs`。

## 計測

- `tools/perf/suite_ab.sh` の交互 A/B（両方の順番、io_uring・epoll、3B と 54KB、平文と TLS）。
- `tools/api-bench`（`BENCH_BASE_DIR`）のネイティブハンドラ。
- strace の 1 要求あたり syscall（io_uring は `io_uring_enter`、epoll は `poll`・`recvfrom`・`epoll_ctl`）。
- アイドル接続のメモリ: B-101 の方法（キープアライブで 1 万接続を保持して RSS / `alloc-stats` の生存ヒープ）で、
  増えるのがワーカーあたりのリング（512KB）だけであること。

## 受け入れ条件

- `tools/api-bench` のネイティブハンドラが io_uring でも tokio 上の axum と同等以上。
- プロキシ（`tools/perf` の交互 A/B）に退行が無く、HTTP/1.1 平文の 3B で改善する。
- アイドル接続あたりのメモリが増えない（増えるのはワーカーごとの固定費だけ）。
- 新しい io_uring オペコード・seccomp の許可を足していない。

## F-206 との関係（2026-10-14）

F-206（Rust の API を axum 以上に、API ライブラリの範囲だけで）の手順 1 は、同じ方法をハンドラだけのリスナーに
`cfg(feature = "handler")` の分岐で入れる案。**本チケットを先に入れれば F-206 の手順 1 は要らない**（計測だけ行い、
F-206 は確保とルーティングの手順 2〜4 に集中する）。順番は F-205 → F-206 を勧める。
