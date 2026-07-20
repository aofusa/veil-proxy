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

static POOL: Lazy<Arc<OffloadPool>> = Lazy::new(|| {
    let pool = Arc::new(OffloadPool {
        queue: Mutex::new(VecDeque::new()),
        cv: Condvar::new(),
    });
    // FS の syscall は短時間なので少数のワーカーで十分。最低 2、最大 8。
    let workers = num_cpus::get().clamp(2, 8);
    for _ in 0..workers {
        let pool = Arc::clone(&pool);
        std::thread::Builder::new()
            .name("veil-fs-offload".to_string())
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
            .expect("failed to spawn veil-fs-offload thread");
    }
    pool
});

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

/// ブロッキングなクロージャ `f` を専用スレッドで実行し、結果を非同期に受け取る。
///
/// ドライバのあるワーカースレッドでは eventfd で待機して**イベントループをブロックしない**。
/// ドライバが無い（単体テスト等）場合は同期インライン実行にフォールバックする。
pub async fn offload<F, R>(f: F) -> R
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
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
            return slot
                .lock()
                .unwrap()
                .take()
                .expect("offload result must be present once done");
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
                return slot
                    .lock()
                    .unwrap()
                    .take()
                    .expect("offload result must be present once done");
            }
            OffloadWait {
                read_fd,
                done: Arc::clone(&done),
            }
            .await;
        }
    }
}
