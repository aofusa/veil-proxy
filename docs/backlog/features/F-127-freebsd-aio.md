# F-127: FreeBSD kqueue AIO（POSIX AIO）実装

- 優先度: P2
- ステータス: **完了**
- 起点: v0.6.0 マルチプラットフォーム対応（F-120 で FreeBSD は kqueue readiness reactor
  のみ対応。POSIX AIO による完了通知ベース I/O は未対応だった）
- 設計: `docs/artifacts/f127_freebsd_aio_design.md`（設計: Fable / 実装: Sonnet）

## 目的

FreeBSD で `aio_read(2)`/`aio_write(2)`（POSIX AIO）を用いた完了通知ベースの
TCP read/write 経路を追加する。既存の kqueue readiness 経路（`EVFILT_READ`/
`EVFILT_WRITE` の try-first）はワークロードによっては AIO より不利になり得るため、
**ビルド時 feature flag（`aio`）でオプトイン**にし、既定は現行の readiness 経路の
まま無改変・ゼロコストで維持する。

## 有効化方式: ビルド時 feature flag `aio`

runtime config ではなくビルド時 feature を選んだ理由（設計ドキュメントより）:

- AIO と readiness は Future の実装そのものが根本的に異なる（submit + completion vs.
  wait-readable + nonblocking syscall）。runtime 切替にすると毎 I/O で分岐が入り
  ホットパスにコストと複雑性を持ち込む。
- feature flag なら `veil_aio` cfg 未設定時に AIO コードが一切コンパイルされず、
  既定 kqueue 経路が完全に無改変・ゼロコストで維持される。
- 既存の `epoll` feature（Linux 専用・非対象ターゲットで build.rs が panic）と
  同じ「プラットフォーム特化ビルド選択」パターンに一致する。

### build.rs

`emit_runtime_backend_cfg()` に追加:

```text
feature = "aio" かつ target_os = "freebsd"  →  cfg(veil_aio) 発行
feature = "aio" かつ それ以外               →  panic（epoll と同様の明確なエラー）
```

`cargo::rustc-check-cfg=cfg(veil_aio)` も発行し `unexpected_cfgs` を防ぐ。
cfg 対応表（build.rs 冒頭のドキュメントコメント）にも `veil_aio` 行を追加した。

### Cargo.toml

```toml
# FreeBSD 専用: POSIX AIO（aio_read/aio_write + EVFILT_AIO）による TCP read/write
# 完了通知ベース経路（F-127）。既定オフ。FreeBSD 以外で指定すると build.rs が panic する。
aio = []
```

`full`/`default` には含めない（`epoll` と同じくプラットフォーム限定のオプトイン）。

## 実装

### モジュール構成（隔離）

- **`src/runtime/reactor/aio.rs`（新規、`#[cfg(veil_aio)]`）**:
  - `AioTable`: スレッドローカルなスラブ（`runtime::uring::executor::OpTable` と
    同型の設計）。token（index + 世代パックの `u64`）で in-flight op を識別する。
  - `AioSlotState::Active { cb: Box<libc::aiocb>, fd, waker, outcome }`:
    submit 済みの op。`cb` は `Box` で保持し、`Vec<AioSlot>` が再配置されても
    ヒープ上の `aiocb` 自体は移動しない（固定アドレス生存の保証）。
  - `AioSlotState::Detached { cb, cleanup }`: Future が完了前に drop され
    `aio_cancel` 発行済みの op。完了 kevent 到着まで `cleanup`（バッファ解放
    クロージャ）を保持する。
  - `submit_read`/`submit_write` に相当する `try_submit_read`/`try_submit_write`:
    `aio_read`/`aio_write` を発行し、`EAGAIN` ならスロットを即座に破棄して
    `SubmitOutcome::Fallback` を返す。
  - `handle_completion(token)`: kevent ループから呼ばれ、`aio_error`/`aio_return`
    で結果を回収（`reap`）してから Waker を起こす、または `Detached` なら
    `cleanup` を実行してスロットを解放する。
  - `AioReadFuture<T: IoBufMut>`/`AioWriteFuture<T: IoBuf>`: 状態機械
    `NotSubmitted → Submitted(token) | Fallback → Done`。`Fallback` 状態は
    既存 `reactor::tcp::ReadFuture`/`WriteFuture` と同一の try-first ループを
    そのまま踏襲する。
- **`src/runtime/reactor/executor.rs`**: kevent ドレインループ（kqueue 版 `park`）に
  `EVFILT_AIO` 分岐を追加（`#[cfg(veil_aio)]` ガード）。`ev.udata` を token として
  `aio::handle_completion` へ渡す。
  - 設計ドキュメントは「`kqueue.rs` のドレインループに追加する」と記載しているが、
    実際のコードベースでは kevent の受信・ディスパッチは `kqueue.rs`（薄い
    `kevent(2)` ラッパのみ）ではなく `executor.rs`（`park`/`dispatch_event`）に
    実装されている。そのため `EVFILT_AIO` 分岐は実際のドレインループがある
    `executor.rs` 側に追加した（設計意図と実装箇所の対応はコード内コメントで
    明記済み）。
  - `current_kqueue_fd()`（`#[cfg(veil_aio)]`）を新設し、AIO submit 時の
    `SIGEV_KEVENT` 通知先 kq fd を取得する。
- **`src/runtime/reactor/kqueue.rs`**: `KqueuePoller::raw_fd()`（`#[cfg(veil_aio)]`）
  を追加し、`current_kqueue_fd()` から内部 kq fd を取得できるようにした。
  既存の `update`/`delete`/`wait`（EVFILT_READ/WRITE readiness 経路）は無改変。
- **`src/runtime/reactor/tcp.rs`**: `TcpStream::read`/`write`（および
  `AsyncReadRent`/`AsyncWriteRent` トレイト実装）を `#[cfg(veil_aio)]` で
  `aio::AioReadFuture`/`AioWriteFuture` 経由に、`#[cfg(not(veil_aio))]` は
  現行の readiness `ReadFuture`/`WriteFuture` のまま維持（cfg による
  コンパイル時二者択一）。scatter-gather 送信（`writev2`/`SendMsgFuture`）と
  `readable()`/`writable()`（純粋な readiness 待ち）は POSIX AIO の対象外の
  ままとした（POSIX AIO は単一バッファの read/write のみを対象とする。
  FreeBSD 固有の `aio_writev`/`aio_readv` の活用は本チケットのスコープ外）。

### aiocb / sigevent（FreeBSD 14.3 実機ヘッダで確認、libc クレートの定義をそのまま利用）

FreeBSD 14.3 VM の `/usr/include/sys/aio.h`・`/usr/include/sys/signal.h`・
`/usr/include/sys/event.h` を確認した結果、`libc` クレート 0.2.186 が
`aiocb`/`sigevent`/`sigval`/`aio_read`/`aio_write`/`aio_return`/`aio_error`/
`aio_cancel`/`SIGEV_KEVENT`/`EVFILT_AIO` を**すべて実機定義と一致する形で
公開済み**であることを確認し、独自 `#[repr(C)]` 定義は不要だった（設計文書の
「libc の定義を優先、無い部分のみ独自定義」の判断基準どおり、今回は libc の
定義のみで完結）。

- `SIGEV_KEVENT` 通知先 kq fd は FreeBSD の慣例で `sigev_signo` フィールドに
  格納する（C ヘッダでは `#define sigev_notify_kqueue sigev_signo` という
  別名マクロだが、`libc` クレートはこの別名を公開していないため
  `sigev_signo` を直接使う。コード内コメントで明記）。
- `sigev_value.sival_ptr` に op token（`u64` を `usize` 経由でポインタへ
  キャスト）を格納し、`man 4 aio` の記載どおり完了 kevent の `udata` に
  そのまま渡ってくることを確認した。

### aio_cancel の扱い（man aio_cancel(2) 準拠）

`aio_cancel(2)` の戻り値（`AIO_CANCELED`/`AIO_NOTCANCELED`/`AIO_ALLDONE`）
いずれの場合も、`aio(4)`/`aio_cancel(2)` の記載どおり
**「Normal asynchronous notification occurs for cancelled requests」**
（SIGEV_KEVENT 登録済みの op には必ず完了通知が届く）ため、本実装では
戻り値による分岐を行わず、常にスロットを `Detached` へ移して完了 kevent の
到着を待ってからバッファを解放する（uring バックエンドの `OpGuard`/
`detach_op` と同型の設計）。

## テスト

### FreeBSD VM 実機検証（14.3-RELEASE amd64）

- **ビルド**: FreeBSD 14.3 VM 上で `cargo build --features aio` がネイティブビルド成功
  （warning 0）。`libc` 0.2.186 の `aiocb`/`sigevent`/`sigval`/`aio_*` 定義がそのまま
  実機ヘッダと一致したため独自 `#[repr(C)]` 定義は不要だった。
- **AIO が実際に使われることの確認（truss）**: veil を TLS フロント → プレーン HTTP
  バックエンド（python `http.server`）のリバースプロキシとして起動し
  （バックエンドレッグは `TcpStream` の `AsyncReadRent`/`AsyncWriteRent` を経由するため
  AIO 対象）、`truss -f` で syscall を記録。`aio_read`/`aio_write`/`aio_return`/
  `aio_error` が発行され、各 aiocb は
  `sigev_notify=SIGEV_KEVENT, sigev_notify_kqueue=<kq fd>` を持ち、`aio_return` が
  実バイト数（例: 102 / 186 / 215）を返すことを確認。小ボディ（HTTP 200・本文 sha256
  一致）と大ボディ（~2–3 MB・複数レコード、HTTP 200・本文 sha256 一致）の両方で
  完了通知経路が正しく動作した。
  - 補足: veil のクライアント側 TLS データ経路（`src/simple_tls.rs`・
    `src/ktls_rustls.rs`）は `readable()`/`writable()` + 生 `libc::read`/`write` を使い
    `TcpStream::read`/`write`（AIO Future）を経由しないため、**HTTPS 静的配信だけでは
    AIO は発火しない**。AIO が実際に通るのはプロキシのバックエンドレッグ等
    `AsyncReadRent`/`AsyncWriteRent` を使う経路である（設計文書の「まず全 read/write を
    AIO 化」という記述は `TcpStream::read`/`write` に対するもので、TLS グルーの生 I/O は
    対象外。この差異は本チケットのスコープ理解として重要）。
- **EAGAIN → readiness フォールバック（truss）**: `sysctl vfs.aio.max_aio_per_proc=0`
  `vfs.aio.max_aio_queue_per_proc=0` で AIO を枯渇させると、各 `aio_read`/`aio_write`
  が `ERR#35`（`EAGAIN`）を返す。直後に `read(fd, ..., 65536) = 65536` の readiness
  フォールバックが発行され、64 KiB チャンク単位で 2.8 MB の大ボディを最後の部分
  チャンク（15165 B）まで取りこぼしなく転送し、**HTTP 200・本文フルサイズ一致**で
  完了することを確認（`aio_read EAGAIN → 生 read 成功` が transfer 全域で繰り返される
  ことを truss で確認）。フォールバックは応答内容を AIO 版と同一に保つ。
- **正常時のフォールバック非発火**: 既定 sysctl 値（`max_aio_per_proc=32`/
  `max_aio_queue_per_proc=256`）では EAGAIN は 0 件で、全 read/write が AIO 経路を通る。
  逐次プロキシでは in-flight AIO 深さが 1 に収まるため、`max_aio_queue_per_proc=1` に
  下げても EAGAIN は発生せず AIO のまま完了する（直列化で遅くなるのみ）。

### Linux 無回帰

- `cargo build`（デフォルト io_uring）・`cargo build --features full`・
  `cargo clippy --lib --bins`・`cargo fmt --check` が warning/error 0 であることを確認
  （`veil_aio` は Linux では発行されないため `reactor/aio.rs` 自体が一切コンパイルされず、
  io_uring / reactor いずれの経路にも AIO コードは混入しない）。
  - 注: `cargo clippy --all-targets` は本ブランチ既存の `benches/http3.rs`
    （`--features http3` 前提のベンチ）が feature 無指定で E0433 になる既知事象があり、
    これは F-127 と無関係（変更前 HEAD でも同一エラー）。lib/bins のみを対象にした
    `cargo clippy --lib --bins` は warning/error 0。

## 受け入れ条件

- `veil_aio` 未設定時は既存の FreeBSD kqueue readiness 経路・Linux io_uring
  経路のいずれにも一切コード上の変化がない（cfg で完全分離、ゼロコスト）。
- `--features aio`（FreeBSD）で warning 0 ビルドが通り、既存 TLS/L4/HTTP E2E が
  readiness ビルドと同じ応答を返す。
- AIO カーネル上限到達（`EAGAIN`）時に readiness へフォールバックし機能継続する。
- Future drop 時の aiocb/バッファ生存管理が UB を起こさない
  （submit 済み op の early drop で `aio_cancel` → Detached → 完了 kevent 到着後に
  解放、という経路が実装されている）。

## 依存・リスク

- POSIX AIO のソケット対応はワークロード依存の性能特性を持つ
  （readiness より遅くなる場合がある）。本チケットでは経路追加とオプトイン化
  までをスコープとし、性能比較は `tools/perf` の別チケットに委ねる。
- `aio_cancel` 後も必ず完了通知が届くという前提は FreeBSD 14.3 の man ページ
  記載に基づく。将来バージョンでの挙動変化は未検証。
- scatter-gather 送信（`writev2`/`SendMsgFuture`）は本チケットの対象外のまま
  readiness 経路に残っている。
