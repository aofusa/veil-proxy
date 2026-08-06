# F-141: FreeBSD kqueue / AIO の最適化

## 目的

`docs/artifacts/f136_platform_design.md` の「F-139: FreeBSD kqueue / AIO の最適化」節
（チケット番号は F-139 が別用途 = 既存
[F-139-wasm-grpc-call-execution.md](F-139-wasm-grpc-call-execution.md) で使われているため、
実装チケットは F-141 とした）に基づき、FreeBSD の kqueue readiness / POSIX AIO 経路を
io_uring 並みに最適化する。対象は `src/runtime/reactor/`（kqueue/aio/tcp/poller/executor）と
`src/udp/`。**Linux io_uring 経路（`src/runtime/uring/`）・Linux `--features epoll` reactor
経路の挙動は一切変更していない**（poller 抽象の下、`cfg(veil_poller_kqueue)` の内側のみ変更）。

## 実施した改修

### 1. kevent の changelist バッチ化（`src/runtime/reactor/kqueue.rs`、最大の効果）

`KqueuePoller` に `changelist: RefCell<Vec<libc::kevent>>` を追加し、`update()`/`delete()`
は **即座に `kevent()` を呼ばず changelist へ積むだけ**にした。次の `wait()` 呼び出しで
changelist と eventlist を **同一の `kevent()` 呼び出し**にまとめて渡す（io_uring の
`submit_and_wait` 相当）。

- **syscall 削減**: 従来は関心変更（`register_read`/`register_write` の都度）ごとに 1 回、
  park の待機ごとに 1 回で「登録変更 N 回 + 待機 1 回」だった syscall 回数が、
  「イベントループ 1 周につき `kevent()` 1 回」に減る。
- 変更適用のエラー（例: 既に close 済みの fd への `EV_DELETE` が `ENOENT` になる）は、
  `nevents > 0` で eventlist に余裕がある限りカーネルが `EV_ERROR` フラグ付き `kevent` として
  eventlist に詰めて返す（man kevent の標準動作）。`executor::park`（kqueue 版）で
  `EV_ERROR` エントリを読み飛ばすようにした。
- fd 番号の close 直後の再利用に対する安全性: changelist は append 順を保つため、
  古い `EV_DELETE`（既に close 済みで実質無効）→ 新しい `EV_ADD` の順で処理され、
  新規登録を誤って壊さない（`kqueue.rs` の `KqueuePoller` 型 doc に根拠を記載）。
- EINTR 時も changelist を清算してよい理由（changelist はイベント待機フェーズに入る前に
  適用される、という libevent 等が依拠する広く確立された kqueue の挙動）も doc に明記。

### 2. `EVFILT_READ` の `data` 活用（部分実装）

`poller::FdRecord` に `read_hint: usize`（kqueue 専用）を追加し、`executor::dispatch_event`
（kqueue 版）が `EVFILT_READ` 発火時に `ev.data`（読み取り可能バイト数の観測値）を記録する。
`executor::take_read_hint(fd)`（consume-once。取得時に 0 へリセットし、無関係な後続呼び出しへ
古い値が漏れないようにする）を追加し、`reactor::tcp::Readable`/`ReadableFd`
（kqueue バックエンドのみ）が「直前の kqueue 起床で readable と判明済みなら、確認用の
`poll(2)` syscall を省略する」最適化に使う。TCP の `Readable` と UDP の
`wait_readable_fd`（`QuicUdpSocket` の recv 系ループが内部で使用）の両方が恩恵を受ける。

**`EV_CLEAR`（edge trigger）+ `EV_DISPATCH` は採用しなかった**: (a) changelist バッチ化に
より registration syscall は既に 1 ループ 1 回に減っているため、oneshot から
`EV_CLEAR|EV_DISPATCH`（+ 再武装に `EV_ENABLE`）へ切り替えても追加の syscall 削減効果が
薄い、(b) `EV_DISPATCH` が FreeBSD/OpenBSD/NetBSD/macOS 全てで同一の意味論・可用性を持つか
本セッションでは検証できない（クロスコンパイル環境の制約、下記「検証の制約」参照）ため、
検証なしに他 BSD の挙動を変えるリスクを取らなかった。

### 3. AIO の拡張（`lio_listio`/`aio_readv`/`aio_writev`）: **未実装**

設計方針に挙げられた 3 点は、いずれも安全に実装できないと判断し見送った（`AGENTS.md`
「検証なしの unsafe 拡大」禁止に抵触するため）:

- **`aio_readv`/`aio_writev`**: FreeBSD の実カーネル ABI では `aio_iov`/`aio_iovcnt` は
  `aio_buf`/`aio_nbytes` と同じメモリ位置を共有する union だが、本プロジェクトが依存する
  vendored `libc` クレート（0.2.186）の `struct aiocb` 定義はこの union を named field として
  公開していない（`aio_buf`/`aio_nbytes` のみが見え、`aio_readv`/`aio_writev` 関数は
  存在するにも関わらず対応するフィールドが無い）。安全に使うには、カーネルへ渡す
  ABI 構造体に対して未検証のバイトオフセット書き込み（unsafe ポインタキャスト）が
  必要になり、実機 FreeBSD で検証できない状態でこれを行うことはできない。
- **`lio_listio`**: 複数 aiocb の一括投入自体は安全に呼び出せるが、**部分失敗時の
  リカバリ**（`lio_listio` が -1 を返した際、どの aiocb が実際にキューされたかは
  POSIX 上 unspecified）を安全に扱うには、未提出の aiocb にのみ `aio_error(2)` を
  呼んでよいかの判定が要る。呼んではいけない aiocb（本当に「未提出」＝カーネルに
  一度も渡っていない）に対して `aio_error` を呼ぶことは UB になり得り、逆に
  実際にはキューされたのに解放してしまうと **カーネルが解放済みメモリへ完了結果を
  書き込む use-after-free** になり得る。実機検証なしにこの分岐を実装するのは
  セキュリティ上のリスクが高すぎると判断し、見送った。
- 上記のいずれも `veil_aio`（`--features aio`）が既定オフの機能であり、現状の
  `aio_read`/`aio_write`（単発、既存 F-127 実装）は無変更のまま安全に動作し続ける。

### 4. UDP の最適化: 既存実装が既に方針を満たすことを確認

`src/udp/socket.rs` の非 Linux版 `recv_mmsg_sync`/`send_mmsg_async`
（`#[cfg(all(not(target_os = "linux"), unix))]`）は、**readiness 1 回につき
`scratch.batch_size()` を上限に非ブロッキング `recvfrom`/`sendto` をループし、
`WouldBlock` に達した時点で即座に打ち切る**実装に既になっており（`count > 0` で
`break`）、設計方針（「readiness 1回につき最大 N 個までループ」）を既に満たしている。

`EVFILT_READ` の `data` を使ってループ回数の上限を動的に絞る改修も検討したが、
呼び出し元（`http3_server.rs`、本チケットの変更対象外）が `recv_mmsg_sync` の
戻り値ではなく「エラー（`WouldBlock` 含む）」を drain 終了条件にしているため、
`recv_mmsg_sync` 内でループを早期に打ち切っても、呼び出し元が直後にもう一度呼んで
結局同じ回数だけ `recvfrom` を試みることになり、**総 syscall 数は変わらない**
（早期リターンする分だけ呼び出し元での再入が増えるだけ）。そのため実装しなかった
（見せかけの最適化を避けた）。item 2 で追加した `read_hint`/`take_read_hint` は
UDP の `wait_readable_fd` からも共有で使われるため、UDP 側にも確認用 `poll(2)`
省略の恩恵は届いている。

### 5. ゼロコピー送信（`src/runtime/reactor/sendfile.rs`、新規、FreeBSD 専用）

FreeBSD の `sendfile(2)` を使ったファイル→ソケットのゼロコピー送信プリミティブ
（`sendfile_once`/`sendfile_all`）を追加した。Linux の `splice`（`target_os = "linux"`
専用）とは完全に独立した実装（`cfg(target_os = "freebsd")`）。

- **`SF_NODISKIO` を必須で付与**: FreeBSD の `sendfile` はソケットが非ブロッキングでも
  ファイル側のページフォールトを同期的にブロックする（ホットパス絶対規則違反になる）ため、
  必ず `SF_NODISKIO` を使う。対象範囲が VM キャッシュ未ロードで `EBUSY` になった場合は
  `runtime::offload`（F-29 の既存スレッドプール）で 1 バイトだけ `pread` してページを
  フォールトインしてから再試行する（イベントループはブロックしない）。
- **`sf_hdtr`（ヘッダ同時送出）は意図的に採用しなかった**: 部分送信時の再試行で
  「ヘッダの送信済み分だけ iovec を前進させ、送信済みなら hdtr を外す」という状態管理が
  必要になり、これを誤るとクライアントへのヘッダ重複送信/欠落というレスポンス破損に
  つながる。実機 FreeBSD でしか検証できない領域のため採用を見送った。ヘッダは従来通り
  `write`/`sendmsg` 経路で送り、ファイル本体（ゼロコピー化の効果が大きい部分）のみを
  本モジュールで送る設計とした。
- **配線済み**（レビュー指摘対応）: `KtlsServerStream`/`SimpleTlsServerStream` に
  `is_plain()`（`TlsMode::Plain` 判定）を追加し、`src/proxy.rs` の
  `handle_sendfile_userspace` 冒頭で `#[cfg(target_os = "freebsd")]` かつ
  `tls_stream.is_plain()`（TLS 終端なしの平文接続）の場合のみ `sendfile_all` を
  使うファストパスへ分岐させた。**`is_plain() == false`（`TlsMode::Rustls`、
  ユーザー空間 TLS）では従来通り read→暗号化→write を使い、`sendfile(2)` は
  絶対に使わない**（ファイルの生バイトを暗号化なしにソケットへ流すと、
  クライアントが TLS ストリームだと信じている接続に平文が混入する重大な
  セキュリティ上の欠陥になるため）。kTLS 有効時（`is_ktls_send_enabled()`）は
  既存の `handle_sendfile_zerocopy`（`sendfile_ktls`、FreeBSD 版は 7 引数 API を
  使用、本チケットの新規実装ではなく既存 F-126 実装）側で処理済みでこの
  ファストパスには到達しない。

## 検証の制約（本セッションでは実施不能）

- 本セッションの実行環境は Linux x86_64 のみで、FreeBSD/OpenBSD の実カーネル・実機
  QEMU は使用できない。`kqueue.rs`/`aio.rs`（既存部分は無変更）/`sendfile.rs` は
  **一度もコンパイルされていない**（Linux では `cfg(target_os = "freebsd")`/
  `cfg(veil_poller_kqueue)` が真にならないため）。
  - `cargo check --target x86_64-unknown-freebsd --no-default-features --features
    full-freebsd` を試したが、`aws-lc-sys` の build script が cross bindgen で
    `bits/libc-header-start.h` を解決できず失敗（FreeBSD sysroot 不足。Cargo.toml の
    target 別依存が「`target_os` のみで決まる無条件の依存」であるため、feature を
    絞っても回避できない）。
  - `cargo check --target x86_64-unknown-openbsd --no-default-features --features
    full-openbsd` は `x86_64-unknown-openbsd` の std 自体が未インストールで
    `core`/`std` が見つからず失敗（依頼元の指示どおり「std 未導入なら報告のみ」とする）。
  - 代わりに、変更した libc 呼び出し（`kevent`/`sendfile`/`pread`/`lio_listio` の
    シグネチャ検討時のみ）はローカルに vendored された `libc-0.2.186` のソース
    （`~/.cargo/registry/src/.../libc-0.2.186/src/unix/bsd/...`）を直接読んで
    フィールド型・関数シグネチャを確認した上で実装した。
- Linux 側（`cargo build --features full`、`cargo build --features "full,epoll"`、
  `cargo test --lib --features full`（795 pass）、`cargo test --lib --features
  "full,epoll"`（780 pass）、`cargo clippy --features full --all-targets -- -D
  warnings`、`cargo clippy --features "full,epoll" --all-targets -- -D warnings`）は
  すべて warning ゼロ・テスト全通過を確認済み（`veil_rt_uring`/`veil_poller_epoll`
  経路は無変更のため regression 無し）。
- 実機 FreeBSD（QEMU）でのビルド・E2E・性能計測は依頼者が別途実施する。
