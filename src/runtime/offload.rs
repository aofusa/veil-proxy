//! ブロッキング FS 操作の非同期オフロード（F-29）
//!
//! `canonicalize`（シンボリックリンク解決を含むパス正規化）は io_uring に対応オペコードが
//! 無く（`readlink` 系が存在しない）、純 io_uring では非同期化できない。また thread-per-core
//! エグゼキュータのタスクは `Send` でないため、ワーカースレッドからタスク Waker を直接
//! 呼ぶのは不健全である。
//!
//! そこで「ブロッキング処理を専用スレッドプールにオフロードし、完了をスレッドごとの
//! `eventfd`（`POLL_ADD` で待機）でイベントループへ通知する」非同期オフロード機構を
//! **一から自作** する。これにより、キャッシュミス時の `canonicalize`/`metadata` を
//! **イベントループをブロックせず** に実行できる。新規 io_uring オペコードは追加しない
//! （`POLL_ADD` は許可済み）ため、セキュリティサーフェスは変わらない。
//!
//! ## 健全性
//!
//! - 待機は起点スレッド自身の `wait_readable_fd(eventfd)`（POLL_ADD）で行う＝同一スレッドで
//!   完結し Waker のクロススレッド呼び出しは発生しない。
//! - ワーカースレッドは結果スロットに値を格納し `done` を `Release` で立て、起点スレッドの
//!   `eventfd` を `write(2)` で叩く（fd への write はスレッド安全）。完了ごとに必ず
//!   eventfd を叩くため取りこぼしは無い。
//! - ランタイムドライバ未初期化（単体テスト等）では同期インライン実行にフォールバックする。

use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::runtime::handle::RawFd;

use once_cell::sync::Lazy;

use crate::runtime::has_driver;
#[cfg(veil_rt_uring)]
use crate::runtime::tcp::wait_readable_fd;

type Job = Box<dyn FnOnce() + Send + 'static>;

/// ブロッキング FS 用スレッドプール（MPMC: Mutex<VecDeque> + Condvar）。
struct OffloadPool {
    queue: Mutex<VecDeque<Job>>,
    cv: Condvar,
}

impl OffloadPool {
    fn submit(&self, job: Job) {
        {
            let mut q = self.queue.lock().unwrap();
            q.push_back(job);
        }
        self.cv.notify_one();
    }
}

impl OffloadPool {
    /// `workers` 本のスレッド（名前 `name`）を持つプールを起動する。
    fn spawn(name: &str, workers: usize) -> Arc<OffloadPool> {
        let pool = Arc::new(OffloadPool {
            queue: Mutex::new(VecDeque::new()),
            cv: Condvar::new(),
        });
        for _ in 0..workers {
            let pool = Arc::clone(&pool);
            std::thread::Builder::new()
                .name(name.to_string())
                .spawn(move || loop {
                    let job = {
                        let mut q = pool.queue.lock().unwrap();
                        while q.is_empty() {
                            q = pool.cv.wait(q).unwrap();
                        }
                        q.pop_front()
                    };
                    if let Some(job) = job {
                        job();
                    }
                })
                .unwrap_or_else(|e| panic!("failed to spawn {} thread: {}", name, e));
        }
        pool
    }
}

// FS の syscall は短時間なので少数のワーカーで十分。最低 2、最大 8。
static POOL: Lazy<Arc<OffloadPool>> =
    Lazy::new(|| OffloadPool::spawn("veil-fs-offload", num_cpus::get().clamp(2, 8)));

/// オフロードワーカープールを事前生成する（スレッド spawn を前倒しする）。
///
/// FreeBSD capability mode（`cap_enter(2)`）へ入った後に初めてオフロードを使うと、
/// その時点でワーカースレッドの遅延生成が走る。cap_enter 後のスレッド生成はスタック
/// ガード設定等で失敗し得るため、**cap_enter 前** に本関数でプールを暖機しておく。
/// 冪等（`Lazy` の初回参照のみが生成を行う）。
pub fn warmup() {
    Lazy::force(&POOL);
}

thread_local! {
    /// 起点スレッドの完了通知用 fd ペア（読み取り fd, 書き込み fd）。初回オフロード時に
    /// 遅延生成する。Linux は eventfd（読み書き同一 fd）、非 Linux（BSD）は
    /// `pipe2(O_NONBLOCK|O_CLOEXEC)` の読み/書き端（F-120 Phase 4: eventfd は Linux 専用
    /// のため、BSD では通知抽象をパイプへ差し替える。設計 3.3 節）。
    static OFFLOAD_NOTIFY: Cell<(RawFd, RawFd)> = const { Cell::new((-1, -1)) };
}

/// 現在のスレッドのオフロード用通知 fd ペア（読み取り fd, 書き込み fd）を取得する
/// （ランタイムドライバがある場合のみ）。Linux では両者は同一の eventfd。
fn current_thread_notify_fds() -> Option<(RawFd, RawFd)> {
    if !has_driver() {
        return None;
    }
    OFFLOAD_NOTIFY.with(|cell| {
        let (r, w) = cell.get();
        if r >= 0 {
            return Some((r, w));
        }
        #[cfg(target_os = "linux")]
        {
            // EFD_NONBLOCK: ドレイン read が即時に返る。EFD_CLOEXEC: exec で漏らさない。
            let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
            if fd < 0 {
                return None;
            }
            cell.set((fd, fd));
            Some((fd, fd))
        }
        // macOS には `pipe2(2)` が無いため `pipe(2)` + `fcntl(F_SETFL, O_NONBLOCK)` +
        // `fcntl(F_SETFD, FD_CLOEXEC)` の 2 段構えへフォールバックする（F-125、設計
        // docs/artifacts/f125_windows_macos_design.md の macOS 節 7）。他 BSD
        // （FreeBSD/OpenBSD）は pipe2 のまま。
        #[cfg(target_os = "macos")]
        {
            let mut fds = [0 as RawFd; 2];
            let ret = unsafe { libc::pipe(fds.as_mut_ptr()) };
            if ret < 0 {
                return None;
            }
            for fd in fds {
                unsafe {
                    libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
                    libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                }
            }
            cell.set((fds[0], fds[1]));
            Some((fds[0], fds[1]))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
        {
            let mut fds = [0 as RawFd; 2];
            let ret = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
            if ret < 0 {
                return None;
            }
            cell.set((fds[0], fds[1]));
            Some((fds[0], fds[1]))
        }
        // Windows には eventfd/pipe2 相当が無いため、ループバック TCP ソケットペアで
        // 自己パイプ相当を実装する（F-125 設計ドキュメント参照）。読み取り fd/書き込み fd
        // は別ソケット（accept 側/connect 側）になる。
        #[cfg(windows)]
        {
            let (r, w) = windows_notify_pair()?;
            cell.set((r, w));
            Some((r, w))
        }
    })
}

/// Windows 版の自己パイプ相当（ループバック TCP ソケットペア）を生成する。
///
/// `127.0.0.1` にバインドした一時リスナーへ即座に接続し、accept 側を読み取り fd、
/// connect 側を書き込み fd として使う。生成はスレッド初回オフロード時のみのコールド
/// パスのため、ブロッキング呼び出し（bind/listen/connect/accept）を使って単純化する
/// （生成後は両ソケットとも非ブロッキングへ切り替える）。
#[cfg(windows)]
// 理由付き allow: スレッド初回オフロード時 1 回だけのコールドパス初期化（eventfd/pipe2
// 生成の Windows 版代替）であり、リクエストごとのホットパスでは呼ばれない。
#[allow(clippy::disallowed_methods)]
fn windows_notify_pair() -> Option<(RawFd, RawFd)> {
    use std::net::{TcpListener, TcpStream};

    let listener = TcpListener::bind("127.0.0.1:0").ok()?;
    let addr = listener.local_addr().ok()?;
    let client = TcpStream::connect(addr).ok()?;
    let (accepted, _) = listener.accept().ok()?;

    // std の TcpStream/TcpListener は Drop で close するため、生ソケットの所有権を
    // こちら側へ移した上で `into_raw_socket()` で取り出す（Drop による二重 close を防ぐ）。
    use std::os::windows::io::IntoRawSocket;
    let read_sock = accepted.into_raw_socket();
    let write_sock = client.into_raw_socket();

    let mut nonblocking: u32 = 1;
    unsafe {
        windows_sys::Win32::Networking::WinSock::ioctlsocket(
            read_sock as windows_sys::Win32::Networking::WinSock::SOCKET,
            windows_sys::Win32::Networking::WinSock::FIONBIO,
            &mut nonblocking,
        );
        windows_sys::Win32::Networking::WinSock::ioctlsocket(
            write_sock as windows_sys::Win32::Networking::WinSock::SOCKET,
            windows_sys::Win32::Networking::WinSock::FIONBIO,
            &mut nonblocking,
        );
    }
    Some((read_sock as RawFd, write_sock as RawFd))
}

/// 通知 fd の完了シグナルをドレインする（best-effort）。
///
/// Linux（eventfd）は 8 バイト固定の 64bit カウンタ読み取りで全完了分を一括ドレインできる。
/// 非 Linux（パイプ）は完了ごとに 1 バイト書き込まれるため、複数完了が溜まっている場合に
/// 備えてまとまった読み取りバッファでドレインする（読み残しがあっても次回 poll で `done`
/// を再検査するため正当性には影響しない。詳細は `OffloadWait::poll` のコメント参照）。
/// ドレインを実行し、1 バイト以上読み取れたら `true` を返す。
#[inline]
#[cfg(not(windows))]
fn drain_notify(read_fd: RawFd) -> bool {
    #[cfg(target_os = "linux")]
    let ret = {
        let mut buf = [0u8; 8];
        unsafe { libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, 8) }
    };
    #[cfg(not(target_os = "linux"))]
    let ret = {
        let mut buf = [0u8; 256];
        unsafe { libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) }
    };
    ret > 0
}

/// 通知シグナルをドレインする（Windows 版: ループバックソケットからの `recv`）。
#[inline]
#[cfg(windows)]
fn drain_notify(read_fd: RawFd) -> bool {
    use crate::runtime::handle::win;
    let mut buf = [0u8; 256];
    let ret = unsafe {
        windows_sys::Win32::Networking::WinSock::recv(
            win::to_socket(read_fd),
            buf.as_mut_ptr(),
            buf.len() as i32,
            0,
        )
    };
    ret > 0
}

/// 完了通知を書き込む（ワーカースレッドから起点スレッドへ）。
///
/// # Safety
/// `write_fd` は呼び出し時点で有効な fd であること（起点スレッドが `offload()` 呼び出し中
/// 保持し続けるため、ジョブ実行完了までクローズされない）。
#[inline]
#[cfg(not(windows))]
unsafe fn signal_notify(write_fd: RawFd) {
    #[cfg(target_os = "linux")]
    {
        let val: u64 = 1;
        libc::write(write_fd, &val as *const u64 as *const libc::c_void, 8);
    }
    #[cfg(not(target_os = "linux"))]
    {
        let val: u8 = 1;
        libc::write(write_fd, &val as *const u8 as *const libc::c_void, 1);
    }
}

/// 完了通知を書き込む（Windows 版: ループバックソケットへの `send`）。
///
/// # Safety
/// `write_fd` は呼び出し時点で有効なソケットハンドルであること。
#[inline]
#[cfg(windows)]
unsafe fn signal_notify(write_fd: RawFd) {
    use crate::runtime::handle::win;
    let val: u8 = 1;
    windows_sys::Win32::Networking::WinSock::send(
        win::to_socket(write_fd),
        &val as *const u8,
        1,
        0,
    );
}

// テスト専用: `offload()` の呼び出し回数を数えるフック（B-65）。
//
// 「両キャッシュヒット時に offload を 1 回も呼ばない」ことを単体テストで直接
// 検証するために使う。
//
// **スレッドローカルにすること**（グローバルにしてはならない）。`cargo test` は
// テストを並列実行するため、グローバルカウンタだと**無関係なテストの offload まで
// 数えてしまい、単体では通るのに全体実行でだけ落ちる**という偽陽性になる
// （実際にそれで 2 度落とした）。カウント対象のテストは `block_on_fresh_thread` で
// 専用スレッドを立てて実行するため、スレッドローカルなら他テストと干渉しない。
//
// 同期/非同期どちらの実行経路でも `offload()` に入った時点で必ずインクリメントする。
// 消費側（`cache::static_file` のテスト）が `cache` feature でのみコンパイルされるため、
// フックも同じ条件に揃える（`cache` 無効ビルドで dead_code 警告になるのを防ぐ。
// `allow(dead_code)` は使わない）。
#[cfg(all(test, feature = "cache"))]
thread_local! {
    /// テスト専用: `offload()` 呼び出し回数（呼び出しスレッド単位）。
    pub(crate) static OFFLOAD_CALL_COUNT: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// テスト専用: 呼び出し回数カウンタ（呼び出しスレッド単位）をリセットする。
#[cfg(all(test, feature = "cache"))]
pub(crate) fn reset_offload_call_count() {
    OFFLOAD_CALL_COUNT.with(|c| c.set(0));
}

/// テスト専用: 現在の呼び出し回数（呼び出しスレッド単位）を取得する。
#[cfg(all(test, feature = "cache"))]
pub(crate) fn offload_call_count() -> u64 {
    OFFLOAD_CALL_COUNT.with(|c| c.get())
}

/// ブロッキングなクロージャ `f` を専用スレッドで実行し、結果を非同期に受け取る。
///
/// ドライバのあるワーカースレッドでは eventfd で待機して**イベントループをブロックしない**。
/// ドライバが無い（単体テスト等）場合は同期インライン実行にフォールバックする。
pub async fn offload<F, R>(f: F) -> R
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    // カウンタ本体と同じ cfg 条件に揃える（`cache` 無効ビルドでは
    // `OFFLOAD_CALL_COUNT` 自体が存在しない）。
    #[cfg(all(test, feature = "cache"))]
    OFFLOAD_CALL_COUNT.with(|c| c.set(c.get() + 1));

    let (read_fd, write_fd) = match current_thread_notify_fds() {
        Some(fds) => fds,
        // ドライバ無し: 同期実行（このパスは単体テスト等のみ）
        None => return f(),
    };

    let slot: Arc<Mutex<Option<R>>> = Arc::new(Mutex::new(None));
    let done = Arc::new(AtomicBool::new(false));
    let slot_w = Arc::clone(&slot);
    let done_w = Arc::clone(&done);

    POOL.submit(Box::new(move || {
        let result = f();
        *slot_w.lock().unwrap() = Some(result);
        // 結果格納を Release で公開してから通知 fd を叩く（起点スレッドは Acquire で読む）。
        done_w.store(true, Ordering::Release);
        // SAFETY: write_fd は起点スレッドが `offload()` 呼び出しの間ずっと保持し続ける
        // （関数末尾までクローズされない）。
        unsafe {
            signal_notify(write_fd);
        }
    }));

    wait_until_done(read_fd, &done).await;
    let result = slot
        .lock()
        .unwrap()
        .take()
        .expect("offload result must be present once done");
    result
}

/// 完了フラグ `done` が立つまで、起点スレッドの通知 fd（`read_fd`）で待つ。
///
/// `offload()` と（`handler` feature 時の）`await_completion()` が共有する待機ループ。
/// 完了側は結果を格納してから `done` を `Release` で立て、通知 fd を叩く。
async fn wait_until_done(read_fd: RawFd, done: &Arc<AtomicBool>) {
    // ===== io_uring バックエンド: POLL_ADD 待機 =====
    //
    // POLL_ADD の完了は eventfd への write 時点で CQE として **各待機者に個別に** 記録される
    // （後から他タスクがカウンタを drain しても、既に発行された完了は失われない）。
    // そのため「wait → drain → done 再確認」の素朴なループで取りこぼしが無い。
    // veil_rt_uring は target_os = "linux" 限定（build.rs）のため read_fd/write_fd は
    // 常に同一の eventfd。
    #[cfg(veil_rt_uring)]
    loop {
        if done.load(Ordering::Acquire) {
            return;
        }
        // eventfd が読み取り可能（=どれかのオフロードが完了）になるまで待つ。
        let _ = wait_readable_fd(read_fd).await;
        drain_notify(read_fd);
    }

    // ===== reactor（readiness）バックエンド: done 検査つき専用 Future で待機 =====
    //
    // eventfd カウンタは全 completions を合算した 1 本のレベル信号のため、あるタスクの
    // drain が **他の待機タスク宛ての通知シグナルごと消費** し得る。消費されたシグナルの
    // 持ち主は「readable 待ち」のままでは二度と起床できない（EPOLLONESHOT のイベントは
    // epoll_wait 時点のレベル再評価で蒸発する）。そのため:
    //
    // 1. 待機 Future の poll で **毎回 `done` を最初に検査**する（どんな起床でも、
    //    readiness の有無に関わらず完了を観測できる）。
    // 2. drain した者は同じ eventfd の残り待機者を全員起こす
    //    （`reactor::executor::wake_all_readers`）。起こされた各タスクは (1) により
    //    自身の done を再確認し、未完了なら再登録して待機に戻る。
    //
    // この 2 つの組で「シグナル横取り + 起床消失」を構造的に排除する
    // （F-120 Phase 2 の E2E `test_f62_wasm_http_call_concurrent_requests` で発見）。
    #[cfg(veil_rt_reactor)]
    {
        struct OffloadWait {
            read_fd: RawFd,
            done: Arc<AtomicBool>,
        }

        impl std::future::Future for OffloadWait {
            type Output = ();

            fn poll(
                self: std::pin::Pin<&mut Self>,
                cx: &mut std::task::Context<'_>,
            ) -> std::task::Poll<()> {
                // (1) どんな起床経路でも、まず完了フラグを観測する。
                if self.done.load(Ordering::Acquire) {
                    return std::task::Poll::Ready(());
                }
                // try-first: 既に readable なら drain して他の待機者へ再配布する。
                #[cfg(not(windows))]
                let now_readable = {
                    let mut pfd = libc::pollfd {
                        fd: self.read_fd,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    let ret = unsafe { libc::poll(&mut pfd, 1, 0) };
                    ret > 0 && pfd.revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0
                };
                #[cfg(windows)]
                let now_readable = {
                    use crate::runtime::handle::win;
                    use windows_sys::Win32::Networking::WinSock::{
                        WSAPoll, POLLERR, POLLHUP, POLLRDNORM, WSAPOLLFD,
                    };
                    let mut pfd = WSAPOLLFD {
                        fd: win::to_socket(self.read_fd),
                        events: POLLRDNORM as i16,
                        revents: 0,
                    };
                    let ret = unsafe { WSAPoll(&mut pfd, 1, 0) };
                    ret > 0 && pfd.revents & ((POLLRDNORM | POLLERR | POLLHUP) as i16) != 0
                };
                if now_readable {
                    let drained = drain_notify(self.read_fd);
                    // (2) drain したシグナルは他タスク宛てかもしれない。全読者を起こして
                    // 各自の done を再確認させる（起こされた側は本 poll の (1) で観測する）。
                    if drained {
                        crate::runtime::executor::wake_all_readers(self.read_fd);
                    }
                    // 呼び出し元ループで done を再確認させる（自分宛てだったかは分からない）。
                    return std::task::Poll::Ready(());
                }
                crate::runtime::executor::register_read(self.read_fd, cx.waker().clone());
                std::task::Poll::Pending
            }
        }

        loop {
            if done.load(Ordering::Acquire) {
                return;
            }
            OffloadWait {
                read_fd,
                done: Arc::clone(done),
            }
            .await;
        }
    }
}

// ===== API ライブラリ用（`handler` feature のみ）: 外部スレッドの完了待ち・ハンドラ専用プール =====

/// 別スレッドで完了する処理の結果を、起点スレッドへ届ける送り手（`await_completion` が渡す）。
///
/// `complete` を呼ばずに drop された場合（完了側の panic 等）も待機側は起きて `None` を受け取る。
#[cfg(feature = "handler-pool")]
pub struct Completer<R> {
    slot: Arc<Mutex<Option<R>>>,
    done: Arc<AtomicBool>,
    /// 起点スレッドの通知 fd。負値はドライバ無し（単体テスト等。待機側はポーリングする）。
    write_fd: RawFd,
    sent: bool,
}

#[cfg(feature = "handler-pool")]
impl<R> Completer<R> {
    /// 結果を格納して起点スレッドを起こす。
    pub fn complete(mut self, value: R) {
        *self.slot.lock().unwrap() = Some(value);
        self.finish();
    }

    fn finish(&mut self) {
        if self.sent {
            return;
        }
        self.sent = true;
        // 結果格納を Release で公開してから通知 fd を叩く（起点スレッドは Acquire で読む）。
        self.done.store(true, Ordering::Release);
        if self.write_fd >= 0 {
            // SAFETY: write_fd は起点スレッドのスレッドローカルな通知 fd で、スレッドの生存中は
            // 閉じられない（`offload()` と同じ前提）。
            unsafe {
                signal_notify(self.write_fd);
            }
        }
    }
}

#[cfg(feature = "handler-pool")]
impl<R> Drop for Completer<R> {
    fn drop(&mut self) {
        self.finish();
    }
}

/// `submit` に [`Completer`] を渡し、別スレッド（tokio ランタイム・専用プール等）で
/// `complete` されるのを、イベントループをブロックせずに待つ。
///
/// スレッド越しに Waker を呼ぶ代わりに `offload()` と同じ「結果スロット + 通知 fd」で
/// 起点スレッドへ戻る（executor のタスクは `!Send`）。ランタイムドライバが無い場合
/// （単体テスト等）は完了までポーリングして待つ。`Completer` が `complete` されずに
/// drop されたら `None`。
#[cfg(feature = "handler-pool")]
pub async fn await_completion<R, S>(submit: S) -> Option<R>
where
    R: Send + 'static,
    S: FnOnce(Completer<R>),
{
    let slot: Arc<Mutex<Option<R>>> = Arc::new(Mutex::new(None));
    let done = Arc::new(AtomicBool::new(false));
    let fds = current_thread_notify_fds();
    submit(Completer {
        slot: Arc::clone(&slot),
        done: Arc::clone(&done),
        write_fd: fds.map(|(_, w)| w).unwrap_or(-1),
        sent: false,
    });
    match fds {
        Some((read_fd, _)) => wait_until_done(read_fd, &done).await,
        None => poll_wait_done(&done),
    }
    let value = slot.lock().unwrap().take();
    value
}

/// ドライバ無し（単体テスト等）のスレッドで完了を待つ。
// 理由付き allow: ランタイムドライバが無いスレッド（イベントループ外の単体テスト）でだけ通る待機。
#[cfg(feature = "handler-pool")]
#[allow(clippy::disallowed_methods)]
fn poll_wait_done(done: &AtomicBool) {
    while !done.load(Ordering::Acquire) {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// 別スレッドから起点スレッド（ワーカー）を起こすための旗（F-210 の C・Python の橋渡し）。
///
/// 起点スレッドで作り（そのスレッドの通知 fd を覚える）、`Arc` で別スレッドへ渡す。別スレッドは
/// [`WakeFlag::notify`] で旗を立てて通知 fd を叩き、起点スレッドは [`WakeFlag::wait`] で旗が立つまで待つ
/// （`await_completion` と同じ「旗 + 通知 fd」。スレッド越しに Waker を呼ばない）。
/// 取りこぼさない使い方: 待つ側は `reset()` → 条件を確かめる → 満たされなければ `wait()`。
#[cfg(feature = "handler-pool")]
pub struct WakeFlag {
    flag: Arc<AtomicBool>,
    read_fd: RawFd,
    write_fd: RawFd,
}

#[cfg(feature = "handler-pool")]
impl WakeFlag {
    /// 現在のスレッド（ワーカー）を起こす旗。ドライバが無い（単体テスト等）ときはポーリングで待つ。
    pub fn new() -> WakeFlag {
        let fds = current_thread_notify_fds();
        WakeFlag {
            flag: Arc::new(AtomicBool::new(false)),
            read_fd: fds.map(|(r, _)| r).unwrap_or(-1),
            write_fd: fds.map(|(_, w)| w).unwrap_or(-1),
        }
    }

    /// 旗を立てて起点スレッドを起こす（どのスレッドからでも）。
    pub fn notify(&self) {
        self.flag.store(true, Ordering::Release);
        if self.write_fd >= 0 {
            // SAFETY: write_fd は起点スレッドのスレッドローカルな通知 fd で、スレッドの生存中は
            // 閉じられない（`Completer` と同じ前提）。
            unsafe { signal_notify(self.write_fd) };
        }
    }

    /// 旗を下ろす（起点スレッドが条件を確かめる前に）。
    pub fn reset(&self) {
        self.flag.store(false, Ordering::Release);
    }

    /// 旗が立つまで待つ（起点スレッドで）。
    pub async fn wait(&self) {
        if self.read_fd >= 0 {
            wait_until_done(self.read_fd, &self.flag).await;
        } else {
            poll_wait_done(&self.flag);
        }
    }
}

#[cfg(feature = "handler-pool")]
impl Default for WakeFlag {
    fn default() -> Self {
        WakeFlag::new()
    }
}

/// ハンドラ用プールの既定スレッド数（CPU 数 × 2、最低 4）。
#[cfg(feature = "handler-pool")]
fn default_handler_threads() -> usize {
    (num_cpus::get() * 2).max(4)
}

/// ハンドラ用プールのスレッド数（`set_handler_pool_threads` で起動前に変更できる）。
#[cfg(feature = "handler-pool")]
static HANDLER_POOL_THREADS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// API ライブラリのブロッキングなハンドラ（Python・C・`offload = true` の Rust）を実行する専用プール。
/// FS 用プール（2〜8 本）と分けるのは、重いハンドラで静的配信のオフロードを枯渇させないため。
#[cfg(feature = "handler-pool")]
static HANDLER_POOL: Lazy<Arc<OffloadPool>> = Lazy::new(|| {
    let n = match HANDLER_POOL_THREADS.load(Ordering::Relaxed) {
        0 => default_handler_threads(),
        n => n,
    };
    OffloadPool::spawn("veil-handler", n)
});

/// ハンドラ用プールのスレッド数を設定する（最初のハンドラ実行より前に呼ぶこと。以後は無効）。
#[cfg(feature = "handler-pool")]
pub fn set_handler_pool_threads(n: usize) {
    HANDLER_POOL_THREADS.store(n, Ordering::Relaxed);
}

/// ブロッキングな処理 `f` をハンドラ用プールで実行し、完了を待たない（結果は呼び出し側が
/// `api::bridge` のキュー等で受け取る。F-210）。`f` の panic はプールのスレッドで捕まえる。
#[cfg(feature = "handler-pool")]
pub fn spawn_handler<F>(f: F)
where
    F: FnOnce() + Send + 'static,
{
    HANDLER_POOL.submit(Box::new(move || {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    }));
}

/// ブロッキングなハンドラ `f` をハンドラ用プールで実行し、結果を非同期に受け取る。
/// `f` が panic したら `None`。
#[cfg(feature = "handler-pool")]
pub async fn offload_handler<F, R>(f: F) -> Option<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    await_completion(move |completer: Completer<R>| {
        HANDLER_POOL.submit(Box::new(move || {
            // panic しても Completer の Drop が待機側を起こす（None）。
            if let Ok(v) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
                completer.complete(v);
            }
        }));
    })
    .await
}
