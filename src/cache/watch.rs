//! 静的配信のキャッシュをディスクの変更に追従させる（F-272、`watch = true`）。
//!
//! ## 世代番号
//!
//! 監視しているルート（`[[route]]` の File バックエンドの `path`）ごとに `AtomicU64` の世代を持つ。
//! `open_file_cache`・`static_file_cache`・圧縮結果のキャッシュのエントリは、作る前（ファイルを開く前）に読んだ
//! 世代（[`Stamp`]）を持ち、当たったときに `stamp.at == gen.load(Relaxed)` を比べるだけで使えるか決める
//! （ホットパスに増えるのは原子変数の読み取り 1 回だけ。syscall は無い）。通知で変更を知った専用スレッドが
//! そのルートの世代を 1 増やすと、そのルートのエントリは次の要求で作り直される（エントリを個別に探さない）。
//! 開く前に読んだ世代を付けるので、読み込みの途中で差し替えられても古い中身が新しい世代で残ることは無い。
//!
//! 有効期限（`valid_duration_secs`）の確認は今までどおり残す。通知が届かない（監視の上限に当たった・
//! 監視できない OS）ときも、期限までに追従する従来の動きに落ちる。
//!
//! ## 通知
//!
//! - Linux: inotify。ルートの配下のディレクトリを辿って付け、作られたディレクトリにも付ける。ルートの
//!   ディレクトリそのものの差し替え（`mv`・シンボリックリンクの付け替え）は親ディレクトリの監視で捉えて
//!   付け直す。通知が溢れた（`IN_Q_OVERFLOW`）ら全部の世代を増やす。
//! - FreeBSD・OpenBSD・NetBSD・macOS: kqueue の `EVFILT_VNODE`。ルートの配下のディレクトリと通常ファイルを
//!   監視スレッドが開いて付ける（ディレクトリの `NOTE_WRITE` で中身を読み直し、増えたものに付ける）。
//!   ファイルの fd で `rename` を伴わない上書きも捉える。開くのは設定ロード時（サンドボックスに入る前）と、
//!   既に持っているディレクトリの fd からの相対（`openat`）だけなので、capsicum・unveil の下でも動く。
//!   持つ fd の数は `RLIMIT_NOFILE` の 1/4 までにし、超えたら警告する。
//! - Windows: `ReadDirectoryChangesW`（再帰）をルートごとの専用スレッドで待つ。親ディレクトリも見て
//!   ルートの差し替えで付け直す。
//! - それ以外の OS: 監視しない（警告して有効期限だけの動きに戻す）。
//!
//! 監視の登録は設定のロード（起動・リロード）のときだけで、データプレーンからは呼ばない。設定から外れた
//! ルートの監視は外さない（プロセスが一度でも設定したルートの数で抑えられる。世代はリロードをまたいで
//! 使い回す）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// 監視しているルートの世代（設定ロード時に [`register`] が返し、ルートの設定に持たせる）。
#[derive(Clone, Copy, Debug)]
pub struct WatchGen(&'static AtomicU64);

/// エントリを作る前に読んだ世代。
#[derive(Clone, Copy, Debug)]
pub struct Stamp {
    gen: &'static AtomicU64,
    at: u64,
}

impl WatchGen {
    /// 今の世代を読む（ファイルを開く前に呼ぶ）。
    #[inline]
    pub fn stamp(self) -> Stamp {
        Stamp {
            gen: self.0,
            at: self.0.load(Ordering::Acquire),
        }
    }

    fn bump(self) {
        self.0.fetch_add(1, Ordering::Release);
    }

    /// テスト用: 今の世代。
    #[cfg(all(test, target_os = "linux"))]
    fn current(self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

/// `gen` があれば今の世代を読む。
#[inline]
pub fn stamp(gen: Option<WatchGen>) -> Option<Stamp> {
    gen.map(WatchGen::stamp)
}

/// エントリが使えるか（有効期限の確認とは別）。
///
/// - 世代を持つエントリ: 作った後にルートが変わっていなければ使える（どのルートの要求からでも）。
/// - 世代を持たないエントリ: 監視しないルートの要求からだけ使える（監視するルートが当てたら作り直して
///   世代を付ける）。
#[inline]
pub fn fresh(stamp: Option<Stamp>, want: Option<WatchGen>) -> bool {
    match stamp {
        Some(s) => s.gen.load(Ordering::Relaxed) == s.at,
        None => want.is_none(),
    }
}

/// 監視しているルート
struct RootEntry {
    path: PathBuf,
    gen: WatchGen,
    /// 監視を付けられた（`false` は失敗して有効期限だけの動きに戻したルート。次のロードでやり直す）
    active: bool,
}

struct State {
    roots: Vec<RootEntry>,
    #[cfg(target_os = "linux")]
    inotify: Option<linux::Inotify>,
    #[cfg(any(
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "macos"
    ))]
    kq: Option<kq::Kq>,
}

static STATE: Mutex<State> = Mutex::new(State {
    roots: Vec::new(),
    #[cfg(target_os = "linux")]
    inotify: None,
    #[cfg(any(
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "macos"
    ))]
    kq: None,
});

fn lock_state() -> std::sync::MutexGuard<'static, State> {
    // 監視スレッドが panic しても登録は続けられるようにする（中身は表と fd だけ）
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// ルート（ディレクトリ、または 1 つのファイル）を監視して世代を返す（設定ロード時に呼ぶ）。
///
/// 監視を付けられなければ警告して `None`（そのルートは有効期限だけで追従する）。同じパスは同じ世代を返す。
pub fn register(root: &Path) -> Option<WatchGen> {
    let mut st = lock_state();
    let idx = match st.roots.iter().position(|r| r.path == root) {
        Some(i) if st.roots[i].active => return Some(st.roots[i].gen),
        Some(i) => i,
        None => {
            let gen = WatchGen(Box::leak(Box::new(AtomicU64::new(0))));
            st.roots.push(RootEntry {
                path: root.to_path_buf(),
                gen,
                active: false,
            });
            st.roots.len() - 1
        }
    };
    match platform_watch(&mut st, idx) {
        Ok(()) => {
            st.roots[idx].active = true;
            // 監視を付ける前に作られたエントリを使わせない
            st.roots[idx].gen.bump();
            Some(st.roots[idx].gen)
        }
        Err(e) => {
            ftlog::warn!(
                "Static file watch for {} is unavailable ({}); cached files follow disk changes only after valid_duration_secs",
                root.display(),
                e
            );
            None
        }
    }
}

/// 監視の途中（新しいディレクトリ・ファイル・ルートの付け直し）で付けられなかったことを知らせる
#[cfg(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "macos"
))]
fn warn_partial(st: &State, root: usize, e: &std::io::Error) {
    if let Some(r) = st.roots.get(root) {
        warn_root(&r.path, e);
    }
}

/// ルートの監視の一部を付けられなかったことを知らせる
#[cfg(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "macos",
    windows
))]
fn warn_root(path: &Path, e: &std::io::Error) {
    ftlog::warn!(
        "Static file watch for {} could not follow a new directory or file ({}); changes there are picked up after valid_duration_secs",
        path.display(),
        e
    );
}

/// 全部のルートの世代を増やす（inotify の通知が溢れたとき）。
#[cfg(any(target_os = "linux", test))]
fn bump_all(st: &State) {
    for r in &st.roots {
        r.gen.bump();
    }
}

#[cfg(target_os = "linux")]
fn platform_watch(st: &mut State, idx: usize) -> std::io::Result<()> {
    linux::watch_root(st, idx)
}

#[cfg(any(
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "macos"
))]
fn platform_watch(st: &mut State, idx: usize) -> std::io::Result<()> {
    kq::watch_root(st, idx)
}

#[cfg(windows)]
fn platform_watch(st: &mut State, idx: usize) -> std::io::Result<()> {
    windows::watch_root(&st.roots[idx].path, st.roots[idx].gen)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "macos",
    windows
)))]
fn platform_watch(_st: &mut State, _idx: usize) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "file change notification is not implemented on this platform",
    ))
}

#[cfg(target_os = "linux")]
mod linux {
    //! inotify の監視（専用スレッドがブロッキングの `read` で受ける。io_uring は使わない）。
    use super::{bump_all, lock_state, warn_partial, State};
    use std::collections::HashMap;
    use std::ffi::{CString, OsStr, OsString};
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::RawFd;
    use std::path::{Path, PathBuf};

    /// 付ける通知（どの watch も同じマスクにする。同じ inode に付け直すとマスクが置き換わるため、
    /// 複数のルートが同じディレクトリを共有しても取りこぼさない）
    const MASK: u32 = libc::IN_CLOSE_WRITE
        | libc::IN_MOVED_TO
        | libc::IN_MOVED_FROM
        | libc::IN_CREATE
        | libc::IN_DELETE
        | libc::IN_ATTRIB
        | libc::IN_DELETE_SELF
        | libc::IN_MOVE_SELF
        | libc::IN_ONLYDIR;

    /// watch の持ち主
    #[derive(Clone, PartialEq, Eq, Debug)]
    enum Owner {
        /// ルートの配下のディレクトリ（ここでの変更はルートの世代を増やす）
        Dir { root: usize },
        /// ルートの親ディレクトリ（`name` の差し替えだけを見て、ルートの監視を付け直す）
        Parent { root: usize, name: OsString },
    }

    struct Watch {
        path: PathBuf,
        owners: Vec<Owner>,
    }

    pub(super) struct Inotify {
        fd: RawFd,
        watches: HashMap<i32, Watch>,
    }

    impl Inotify {
        fn add(&mut self, path: &Path, owner: Owner) -> io::Result<()> {
            let c = CString::new(path.as_os_str().as_bytes())
                .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
            // SAFETY: fd は inotify_init1 が返した fd、c は NUL 終端の C 文字列
            let wd = unsafe { libc::inotify_add_watch(self.fd, c.as_ptr(), MASK) };
            if wd < 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::ENOSPC) {
                    // 監視の上限（ENOSPC は「ディスクが一杯」と読めて紛らわしいので言い換える）
                    return Err(io::Error::other(
                        "inotify watch limit reached; raise fs.inotify.max_user_watches",
                    ));
                }
                return Err(e);
            }
            let w = self.watches.entry(wd).or_insert_with(|| Watch {
                path: path.to_path_buf(),
                owners: Vec::new(),
            });
            if !w.owners.contains(&owner) {
                w.owners.push(owner);
            }
            Ok(())
        }

        /// ルートの持つ watch を外す（他のルートも持つ watch は残す）
        fn remove_root(&mut self, root: usize) {
            let fd = self.fd;
            self.watches.retain(|wd, w| {
                w.owners.retain(|o| match o {
                    Owner::Dir { root: r } | Owner::Parent { root: r, .. } => *r != root,
                });
                if w.owners.is_empty() {
                    // SAFETY: fd・wd は inotify が返した値（既に外れていても EINVAL が返るだけ）
                    unsafe { libc::inotify_rm_watch(fd, *wd) };
                    false
                } else {
                    true
                }
            });
        }

        /// `dir` とその配下のディレクトリ全部に付ける（シンボリックリンクのディレクトリは辿らない）
        // 理由付き allow: 設定ロード時・監視スレッドでだけ呼ぶ（データプレーンの外）。
        #[allow(clippy::disallowed_methods)]
        fn add_tree(&mut self, dir: &Path, root: usize) -> io::Result<()> {
            let mut stack = vec![dir.to_path_buf()];
            while let Some(d) = stack.pop() {
                match self.add(&d, Owner::Dir { root }) {
                    Ok(()) => {}
                    // 辿っている間に消えたディレクトリは飛ばす
                    Err(e) if e.raw_os_error() == Some(libc::ENOENT) && d != dir => continue,
                    Err(e) => return Err(e),
                }
                let Ok(rd) = std::fs::read_dir(&d) else {
                    continue;
                };
                for ent in rd.flatten() {
                    if ent.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        stack.push(ent.path());
                    }
                }
            }
            Ok(())
        }
    }

    fn ensure_inotify(st: &mut State) -> io::Result<&mut Inotify> {
        if st.inotify.is_none() {
            // SAFETY: 引数はフラグだけ。ブロッキングの fd（監視スレッドが read で待つ）
            let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
            if fd < 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() == Some(libc::EMFILE) {
                    return Err(io::Error::other(
                        "inotify instance limit reached; raise fs.inotify.max_user_instances",
                    ));
                }
                return Err(e);
            }
            std::thread::Builder::new()
                .name("veil-fswatch".into())
                .spawn(move || run(fd))?;
            st.inotify = Some(Inotify {
                fd,
                watches: HashMap::new(),
            });
        }
        Ok(st.inotify.as_mut().expect("inotify is initialized above"))
    }

    // 理由付き allow: 設定ロード時に呼ぶ（データプレーンの外）。
    #[allow(clippy::disallowed_methods)]
    pub(super) fn watch_root(st: &mut State, idx: usize) -> io::Result<()> {
        let path = st.roots[idx].path.clone();
        let ino = ensure_inotify(st)?;
        ino.remove_root(idx);
        let res = attach(ino, &path, idx);
        if res.is_err() {
            ino.remove_root(idx);
        }
        res
    }

    // 理由付き allow: 設定ロード時・監視スレッドでだけ呼ぶ（データプレーンの外）。
    #[allow(clippy::disallowed_methods)]
    fn attach(ino: &mut Inotify, path: &Path, idx: usize) -> io::Result<()> {
        let meta = std::fs::metadata(path)?;
        let parent = path
            .parent()
            .map(|p| {
                if p.as_os_str().is_empty() {
                    Path::new(".")
                } else {
                    p
                }
            })
            .unwrap_or(Path::new("."));
        if meta.is_dir() {
            ino.add_tree(path, idx)?;
            // ルートのディレクトリそのものの差し替え（`mv`・シンボリックリンクの付け替え）を捉える。
            // 親を監視できなくても配下の監視は効くので失敗は無視する。
            if let Some(name) = path.file_name() {
                let _ = ino.add(
                    parent,
                    Owner::Parent {
                        root: idx,
                        name: name.to_os_string(),
                    },
                );
            }
        } else {
            // 1 つのファイルのルート: 親ディレクトリの変更で増やす（差し替えは親ディレクトリの rename で届く）
            ino.add(parent, Owner::Dir { root: idx })?;
        }
        Ok(())
    }

    /// 監視スレッドの本体
    fn run(fd: RawFd) {
        // inotify_event は 4 バイト境界に置く
        let mut buf = vec![0u64; 8192];
        let cap = buf.len() * 8;
        loop {
            // SAFETY: buf は cap バイトの書き込み可能な領域
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), cap) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                ftlog::warn!(
                    "Static file watch stopped ({}); cached files follow disk changes only after valid_duration_secs",
                    e
                );
                return;
            }
            // SAFETY: read が書いた先頭 n バイト
            let bytes =
                unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), n as usize) };
            let mut st = lock_state();
            for ev in parse(bytes) {
                handle(&mut st, ev);
            }
        }
    }

    /// 読み取った 1 件
    #[derive(Debug, PartialEq, Eq)]
    pub(super) struct Event<'a> {
        pub(super) wd: i32,
        pub(super) mask: u32,
        pub(super) name: &'a [u8],
    }

    /// read で得た領域を通知の列にする
    pub(super) fn parse(bytes: &[u8]) -> Vec<Event<'_>> {
        let head = std::mem::size_of::<libc::inotify_event>();
        let mut out = Vec::new();
        let mut off = 0;
        while off + head <= bytes.len() {
            // SAFETY: off + head <= len を確かめた。境界を仮定しない読み取り
            let ev: libc::inotify_event =
                unsafe { std::ptr::read_unaligned(bytes.as_ptr().add(off).cast()) };
            let start = off + head;
            let end = (start + ev.len as usize).min(bytes.len());
            let raw = &bytes[start..end];
            let name = match raw.iter().position(|&b| b == 0) {
                Some(i) => &raw[..i],
                None => raw,
            };
            out.push(Event {
                wd: ev.wd,
                mask: ev.mask,
                name,
            });
            off = end;
        }
        out
    }

    pub(super) fn handle(st: &mut State, ev: Event<'_>) {
        if ev.mask & libc::IN_Q_OVERFLOW != 0 {
            bump_all(st);
            return;
        }
        let Some(ino) = st.inotify.as_mut() else {
            return;
        };
        let Some(w) = ino.watches.get(&ev.wd) else {
            return;
        };
        let dir = w.path.clone();
        let owners = w.owners.clone();
        if ev.mask & libc::IN_IGNORED != 0 {
            // カーネルが外した（ディレクトリが消えた）
            ino.watches.remove(&ev.wd);
        }
        let name = OsStr::from_bytes(ev.name);
        let created_dir = ev.mask & libc::IN_ISDIR != 0
            && ev.mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0
            && !ev.name.is_empty();
        for owner in owners {
            match owner {
                Owner::Dir { root } => {
                    if let Some(r) = st.roots.get(root) {
                        r.gen.bump();
                    }
                    if created_dir {
                        let ino = st.inotify.as_mut().expect("inotify exists");
                        if let Err(e) = ino.add_tree(&dir.join(name), root) {
                            warn_partial(st, root, &e);
                        }
                    }
                }
                Owner::Parent { root, name: want } => {
                    if name != want.as_os_str() {
                        continue;
                    }
                    let Some(r) = st.roots.get(root) else {
                        continue;
                    };
                    r.gen.bump();
                    if ev.mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0 {
                        // ルートが差し替わった: 新しいディレクトリに付け直す
                        let path = r.path.clone();
                        let ino = st.inotify.as_mut().expect("inotify exists");
                        ino.remove_root(root);
                        if let Err(e) = attach(ino, &path, root) {
                            warn_partial(st, root, &e);
                        }
                        // 付け直す間の変更を取りこぼさない
                        if let Some(r) = st.roots.get(root) {
                            r.gen.bump();
                        }
                    }
                }
            }
        }
    }

    /// テスト用: 通知の領域を組み立てる
    #[cfg(test)]
    pub(super) fn encode(wd: i32, mask: u32, name: &[u8]) -> Vec<u8> {
        let mut padded = name.to_vec();
        padded.push(0);
        while !padded.len().is_multiple_of(4) {
            padded.push(0);
        }
        let ev = libc::inotify_event {
            wd,
            mask,
            cookie: 0,
            len: padded.len() as u32,
        };
        let mut out = Vec::new();
        // SAFETY: inotify_event は POD
        out.extend_from_slice(unsafe {
            std::slice::from_raw_parts(
                (&ev as *const libc::inotify_event).cast::<u8>(),
                std::mem::size_of::<libc::inotify_event>(),
            )
        });
        out.extend_from_slice(&padded);
        out
    }
}

#[cfg(any(
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "macos"
))]
mod kq {
    //! kqueue の `EVFILT_VNODE` の監視（専用スレッドがブロッキングの `kevent` で受ける）。
    //!
    //! ルートの配下のディレクトリと通常ファイルを開いて付ける。ディレクトリの `NOTE_WRITE`（名前の追加・削除・
    //! 差し替え）でそのディレクトリを読み直し、増えた・inode が変わったものに付け直し、消えたものの fd を閉じる。
    //! ファイルの fd は `rename` を伴わない上書き（`NOTE_WRITE`・`NOTE_EXTEND`）を捉える。ルートの親ディレクトリも
    //! 見て、ルートそのものの差し替え（`mv`・シンボリックリンクの付け替え）で付け直す。
    //!
    //! 開くのは設定ロード時の絶対パスと、既に持っているディレクトリの fd からの相対（`openat`・`fstatat`・
    //! `fdopendir`）だけ。capsicum の capability mode・OpenBSD の unveil の下でも、監視スレッドの読み直しは動く。
    use super::{lock_state, warn_partial, State};
    use std::collections::HashMap;
    use std::ffi::{CStr, CString, OsStr, OsString};
    use std::io;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::RawFd;
    use std::path::Path;

    /// 付ける通知
    const FFLAGS: u32 = libc::NOTE_WRITE
        | libc::NOTE_DELETE
        | libc::NOTE_RENAME
        | libc::NOTE_ATTRIB
        | libc::NOTE_EXTEND;

    /// 監視のために開く fd の上限を決める `RLIMIT_NOFILE`（ソフト）の割合の分母
    const FD_BUDGET_DIVISOR: u64 = 4;

    /// (st_dev, st_ino)
    type FileId = (u64, u64);

    enum Kind {
        /// ルートの配下のディレクトリ（名前 → 子の添字）
        Dir { children: HashMap<OsString, usize> },
        /// ルートの配下の通常ファイル（1 つのファイルのルートもこれ）
        File,
        /// ルートの親ディレクトリ（`name` の差し替えだけを見る）
        Parent { name: OsString },
    }

    struct Node {
        fd: RawFd,
        root: usize,
        id: FileId,
        kind: Kind,
    }

    /// ルートごとの最上位の添字
    #[derive(Default, Clone, Copy)]
    struct Tops {
        content: Option<usize>,
        parent: Option<usize>,
    }

    pub(super) struct Kq {
        kq: RawFd,
        /// kevent の `udata` はこの添字
        nodes: Vec<Option<Node>>,
        free: Vec<usize>,
        tops: HashMap<usize, Tops>,
        /// 持っている fd の数
        held: usize,
        /// 上限に当たったことを警告したルート
        limit_warned: Vec<usize>,
    }

    fn cstr(name: &OsStr) -> io::Result<CString> {
        CString::new(name.as_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
    }

    /// 監視用に開く（読み書きしない。macOS は `O_EVTONLY` でボリュームの取り外しを妨げない）
    fn open_flags(dir: bool) -> libc::c_int {
        #[cfg(target_os = "macos")]
        let base = libc::O_EVTONLY;
        #[cfg(not(target_os = "macos"))]
        let base = libc::O_RDONLY;
        base | libc::O_CLOEXEC | if dir { libc::O_DIRECTORY } else { 0 }
    }

    fn fstat_id(fd: RawFd) -> io::Result<(FileId, bool, bool)> {
        // SAFETY: stat は書き込み先。fd は呼び出し元が開いた fd
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(fd, &mut st) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(stat_parts(&st))
    }

    /// (id, ディレクトリか, 通常ファイルか)
    // 理由付き allow: st_dev・st_ino の型は OS ごとに違う（macOS の st_dev は i32、FreeBSD は u64）
    #[allow(clippy::unnecessary_cast)]
    fn stat_parts(st: &libc::stat) -> (FileId, bool, bool) {
        let fmt = st.st_mode & libc::S_IFMT;
        (
            (st.st_dev as u64, st.st_ino as u64),
            fmt == libc::S_IFDIR,
            fmt == libc::S_IFREG,
        )
    }

    /// `dirfd` の中の `name` をシンボリックリンクを辿らずに調べる
    fn stat_at(dirfd: RawFd, name: &CStr) -> io::Result<(FileId, bool, bool)> {
        // SAFETY: name は NUL 終端、st は書き込み先
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatat(dirfd, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(stat_parts(&st))
    }

    fn close(fd: RawFd) {
        // SAFETY: 監視が開いて持っている fd（閉じると kqueue の登録も外れる）
        unsafe { libc::close(fd) };
    }

    /// ディレクトリの名前の一覧（`.`・`..` を除く）。fd を複製して読むので元の fd は閉じない
    fn list_dir(fd: RawFd) -> io::Result<Vec<OsString>> {
        // SAFETY: fd は開いているディレクトリの fd
        let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if dup < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: dup は複製した fd（fdopendir が持ち主になり closedir で閉じる）
        let dir = unsafe { libc::fdopendir(dup) };
        if dir.is_null() {
            let e = io::Error::last_os_error();
            close(dup);
            return Err(e);
        }
        // 複製は元の fd と読み取り位置を共有するので先頭に戻す
        // SAFETY: dir は fdopendir が返した DIR
        unsafe { libc::rewinddir(dir) };
        let mut out = Vec::new();
        loop {
            // SAFETY: dir は有効な DIR。返る dirent は次の readdir まで有効
            let ent = unsafe { libc::readdir(dir) };
            if ent.is_null() {
                break;
            }
            // SAFETY: d_name は NUL 終端
            let name = unsafe { CStr::from_ptr((*ent).d_name.as_ptr()) };
            let b = name.to_bytes();
            if b == b"." || b == b".." {
                continue;
            }
            out.push(OsStr::from_bytes(b).to_os_string());
        }
        // SAFETY: fdopendir が返した DIR（dup も閉じる）
        unsafe { libc::closedir(dir) };
        Ok(out)
    }

    // 理由付き allow: rlim_t は OS ごとに違う（FreeBSD は i64、他は u64）
    #[allow(clippy::unnecessary_cast)]
    fn fd_budget() -> usize {
        // SAFETY: rl は書き込み先
        let mut rl: libc::rlimit = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) } != 0 {
            return 256;
        }
        let cur = rl.rlim_cur as u64;
        if rl.rlim_cur == libc::RLIM_INFINITY {
            return 65536;
        }
        (cur / FD_BUDGET_DIVISOR).max(16) as usize
    }

    /// 登録する kevent の構造体を作る（`filter`・`flags` の型は OS ごとに違う（NetBSD は `u32`）が、
    /// libc の定数はそれぞれの型で定義されているのでそのまま入れる）
    fn vnode_kevent(fd: RawFd, udata: usize) -> libc::kevent {
        // SAFETY: kevent は POD
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        ev.ident = fd as libc::uintptr_t;
        ev.filter = libc::EVFILT_VNODE;
        ev.flags = libc::EV_ADD | libc::EV_CLEAR;
        ev.fflags = FFLAGS;
        ev.udata = udata as _;
        ev
    }

    #[cfg(target_os = "netbsd")]
    type KeventCount = libc::size_t;
    #[cfg(not(target_os = "netbsd"))]
    type KeventCount = libc::c_int;

    impl Kq {
        fn new() -> io::Result<Self> {
            // SAFETY: 引数なし
            let kq = unsafe { libc::kqueue() };
            if kq < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: kq は開いた fd
            unsafe { libc::fcntl(kq, libc::F_SETFD, libc::FD_CLOEXEC) };
            Ok(Self {
                kq,
                nodes: Vec::new(),
                free: Vec::new(),
                tops: HashMap::new(),
                held: 0,
                limit_warned: Vec::new(),
            })
        }

        /// 開いた fd を登録する（失敗したら fd を閉じる）
        fn add(&mut self, fd: RawFd, root: usize, id: FileId, kind: Kind) -> io::Result<usize> {
            if self.held >= fd_budget() {
                close(fd);
                return Err(io::Error::other(
                    "kqueue watch limit reached (a quarter of RLIMIT_NOFILE); raise the open file limit",
                ));
            }
            let idx = match self.free.pop() {
                Some(i) => i,
                None => {
                    self.nodes.push(None);
                    self.nodes.len() - 1
                }
            };
            let ev = vnode_kevent(fd, idx);
            // SAFETY: changelist は 1 件、eventlist は空（登録だけ。待たない）
            let r =
                unsafe { libc::kevent(self.kq, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
            if r < 0 {
                let e = io::Error::last_os_error();
                close(fd);
                self.free.push(idx);
                return Err(e);
            }
            self.nodes[idx] = Some(Node { fd, root, id, kind });
            self.held += 1;
            Ok(idx)
        }

        /// 添字の配下を全部閉じる
        fn close_tree(&mut self, idx: usize) {
            let mut stack = vec![idx];
            while let Some(i) = stack.pop() {
                let Some(node) = self.nodes.get_mut(i).and_then(Option::take) else {
                    continue;
                };
                if let Kind::Dir { children } = node.kind {
                    stack.extend(children.into_values());
                }
                close(node.fd);
                self.held -= 1;
                self.free.push(i);
            }
        }

        fn remove_root(&mut self, root: usize) {
            if let Some(t) = self.tops.remove(&root) {
                for i in [t.content, t.parent].into_iter().flatten() {
                    self.close_tree(i);
                }
            }
        }

        /// ディレクトリ `idx` を読み直し、増えた・差し替わった子に付け、消えた子を閉じる（増えたディレクトリは
        /// 配下も辿る）。付けられなかったものがあれば最初のエラーを返す（他は続ける）。
        fn rescan(&mut self, idx: usize) -> io::Result<()> {
            let mut first_err: Option<io::Error> = None;
            let mut stack = vec![idx];
            while let Some(di) = stack.pop() {
                let Some(Node {
                    fd: dirfd, root, ..
                }) = self.nodes.get(di).and_then(Option::as_ref)
                else {
                    continue;
                };
                let (dirfd, root) = (*dirfd, *root);
                let names = match list_dir(dirfd) {
                    Ok(n) => n,
                    Err(e) => {
                        first_err.get_or_insert(e);
                        continue;
                    }
                };
                // 今の子（名前 → (添字, id)）
                let old: Vec<(OsString, usize)> = match self.nodes[di].as_ref().map(|n| &n.kind) {
                    Some(Kind::Dir { children }) => {
                        children.iter().map(|(k, v)| (k.clone(), *v)).collect()
                    }
                    _ => continue,
                };
                let mut keep: HashMap<OsString, usize> = HashMap::with_capacity(names.len());
                for name in names {
                    let Ok(c) = cstr(&name) else { continue };
                    let Ok((id, is_dir, is_reg)) = stat_at(dirfd, &c) else {
                        continue;
                    };
                    if !is_dir && !is_reg {
                        // シンボリックリンク・特殊ファイルは辿らない
                        continue;
                    }
                    if let Some(ci) = old.iter().find(|(n, _)| *n == name).map(|x| x.1) {
                        if self.nodes[ci].as_ref().is_some_and(|n| n.id == id) {
                            keep.insert(name, ci);
                            continue;
                        }
                    }
                    // SAFETY: c は NUL 終端の相対名、dirfd は持っているディレクトリの fd
                    let fd = unsafe {
                        libc::openat(dirfd, c.as_ptr(), open_flags(is_dir) | libc::O_NOFOLLOW)
                    };
                    if fd < 0 {
                        let e = io::Error::last_os_error();
                        // 読み直しの間に消えたものは飛ばす
                        if e.raw_os_error() != Some(libc::ENOENT) {
                            first_err.get_or_insert(e);
                        }
                        continue;
                    }
                    // 開いた後に差し替わっていたら開いたものの id を使う
                    let id = fstat_id(fd).map(|x| x.0).unwrap_or(id);
                    let kind = if is_dir {
                        Kind::Dir {
                            children: HashMap::new(),
                        }
                    } else {
                        Kind::File
                    };
                    match self.add(fd, root, id, kind) {
                        Ok(ci) => {
                            if is_dir {
                                stack.push(ci);
                            }
                            keep.insert(name, ci);
                        }
                        Err(e) => {
                            first_err.get_or_insert(e);
                        }
                    }
                }
                // 残らなかった子（消えた・差し替わった）を閉じる
                for (name, ci) in old {
                    if keep.get(&name) != Some(&ci) {
                        self.close_tree(ci);
                    }
                }
                if let Some(Node {
                    kind: Kind::Dir { children },
                    ..
                }) = self.nodes[di].as_mut()
                {
                    *children = keep;
                }
            }
            match first_err {
                Some(e) => Err(e),
                None => Ok(()),
            }
        }

        /// ルートの中身（ディレクトリなら配下全部、ファイルならそれ）に付ける。`at` は親の fd と名前
        /// （付け直し。capability mode でも開ける）、`None` は絶対パスで開く（設定ロード時）。
        fn attach_content(
            &mut self,
            root: usize,
            path: &Path,
            at: Option<(RawFd, &CStr)>,
        ) -> io::Result<()> {
            let fd = match at {
                // SAFETY: name は NUL 終端の相対名、pfd は持っている親ディレクトリの fd
                Some((pfd, name)) => unsafe { libc::openat(pfd, name.as_ptr(), open_flags(false)) },
                None => {
                    let c = cstr(path.as_os_str())?;
                    // SAFETY: c は NUL 終端のパス
                    unsafe { libc::open(c.as_ptr(), open_flags(false)) }
                }
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            let (id, is_dir, _) = match fstat_id(fd) {
                Ok(x) => x,
                Err(e) => {
                    close(fd);
                    return Err(e);
                }
            };
            let kind = if is_dir {
                Kind::Dir {
                    children: HashMap::new(),
                }
            } else {
                Kind::File
            };
            let idx = self.add(fd, root, id, kind)?;
            self.tops.entry(root).or_default().content = Some(idx);
            if is_dir {
                self.rescan(idx)?;
            }
            Ok(())
        }
    }

    fn ensure_kq(st: &mut State) -> io::Result<&mut Kq> {
        if st.kq.is_none() {
            let kq = Kq::new()?;
            let fd = kq.kq;
            if let Err(e) = std::thread::Builder::new()
                .name("veil-fswatch".into())
                .spawn(move || run(fd))
            {
                close(fd);
                return Err(e);
            }
            st.kq = Some(kq);
        }
        Ok(st.kq.as_mut().expect("kqueue is initialized above"))
    }

    pub(super) fn watch_root(st: &mut State, idx: usize) -> io::Result<()> {
        let path = st.roots[idx].path.clone();
        let kq = ensure_kq(st)?;
        kq.remove_root(idx);
        let res = kq.attach_content(idx, &path, None);
        if res.is_err() {
            kq.remove_root(idx);
            return res;
        }
        // ルートそのものの差し替えを捉える（親を監視できなくても配下の監視は効くので失敗は無視する）
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p,
            _ => Path::new("."),
        };
        if let (Some(name), Ok(c)) = (path.file_name(), cstr(parent.as_os_str())) {
            // SAFETY: c は NUL 終端のパス
            let fd = unsafe { libc::open(c.as_ptr(), open_flags(true)) };
            if fd >= 0 {
                if let Ok((id, _, _)) = fstat_id(fd) {
                    if let Ok(pi) = kq.add(
                        fd,
                        idx,
                        id,
                        Kind::Parent {
                            name: name.to_os_string(),
                        },
                    ) {
                        kq.tops.entry(idx).or_default().parent = Some(pi);
                    }
                } else {
                    close(fd);
                }
            }
        }
        Ok(())
    }

    /// 監視スレッドの本体
    fn run(kq: RawFd) {
        // SAFETY: kevent は POD
        let mut evs: Vec<libc::kevent> = vec![unsafe { std::mem::zeroed() }; 64];
        loop {
            // SAFETY: eventlist は evs の長さ分の書き込み可能な領域。timeout は無し（ブロッキング）
            let n = unsafe {
                libc::kevent(
                    kq,
                    std::ptr::null(),
                    0,
                    evs.as_mut_ptr(),
                    evs.len() as KeventCount,
                    std::ptr::null(),
                )
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                ftlog::warn!(
                    "Static file watch stopped ({}); cached files follow disk changes only after valid_duration_secs",
                    e
                );
                return;
            }
            let mut st = lock_state();
            for ev in &evs[..n as usize] {
                if ev.flags & libc::EV_ERROR != 0 {
                    continue;
                }
                handle(&mut st, ev.udata as usize, ev.fflags);
            }
        }
    }

    fn handle(st: &mut State, idx: usize, fflags: u32) {
        let Some(kq) = st.kq.as_mut() else {
            return;
        };
        let Some(node) = kq.nodes.get(idx).and_then(Option::as_ref) else {
            return;
        };
        let (root, pfd) = (node.root, node.fd);
        let (is_dir, parent_name) = match &node.kind {
            Kind::File => (false, None),
            Kind::Dir { .. } => (true, None),
            Kind::Parent { name } => (false, Some(name.clone())),
        };
        let Some(name) = parent_name else {
            if let Some(r) = st.roots.get(root) {
                r.gen.bump();
            }
            if is_dir && fflags & libc::NOTE_WRITE != 0 {
                let res = kq.rescan(idx);
                // 読み直しの間の変更を取りこぼさない
                if let Some(r) = st.roots.get(root) {
                    r.gen.bump();
                }
                if let Err(e) = res {
                    warn_limit_once(st, root, &e);
                }
            }
            return;
        };
        let Ok(c) = cstr(&name) else { return };
        let now = stat_at(pfd, &c).ok().map(|x| x.0);
        let content = kq.tops.get(&root).and_then(|t| t.content);
        let cur = content.and_then(|i| kq.nodes[i].as_ref()).map(|n| n.id);
        if now == cur {
            return;
        }
        // ルートが差し替わった（または消えた）: 中身の監視を付け直す
        if let Some(ci) = content {
            kq.close_tree(ci);
            if let Some(t) = kq.tops.get_mut(&root) {
                t.content = None;
            }
        }
        let Some(path) = st.roots.get(root).map(|r| r.path.clone()) else {
            return;
        };
        let res = match now {
            Some(_) => kq.attach_content(root, &path, Some((pfd, &c))),
            None => Ok(()),
        };
        if let Some(r) = st.roots.get(root) {
            r.gen.bump();
        }
        if let Err(e) = res {
            warn_limit_once(st, root, &e);
        }
    }

    /// 付けられなかったことをルートごとに 1 回だけ知らせる（上限に当たると読み直しのたびに起きるため）
    fn warn_limit_once(st: &mut State, root: usize, e: &io::Error) {
        let Some(kq) = st.kq.as_mut() else { return };
        if kq.limit_warned.contains(&root) {
            return;
        }
        kq.limit_warned.push(root);
        warn_partial(st, root, e);
    }
}

#[cfg(windows)]
mod windows {
    //! `ReadDirectoryChangesW` の監視（ルートごとの専用スレッド）。
    //!
    //! ルートのディレクトリを配下ごと（`bWatchSubtree`）見て、何か届いたら世代を増やす（何が変わったかは見ない）。
    //! バッファが溢れた（0 バイトで完了）ときも増やす。ルートの親ディレクトリも見て、ルートと同じ名前の
    //! 追加・改名（`mv` の差し替え）で、ルートのハンドルを開き直す。1 つのファイルのルートは親ディレクトリを
    //! 見る（親の中の変更で増やす）。
    use super::WatchGen;
    use std::ffi::OsString;
    use std::io;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::path::{Path, PathBuf};
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_IO_PENDING, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, ReadDirectoryChangesW, FILE_ACTION_ADDED, FILE_ACTION_RENAMED_NEW_NAME,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OVERLAPPED, FILE_LIST_DIRECTORY,
        FILE_NOTIFY_CHANGE_ATTRIBUTES, FILE_NOTIFY_CHANGE_CREATION, FILE_NOTIFY_CHANGE_DIR_NAME,
        FILE_NOTIFY_CHANGE_FILE_NAME, FILE_NOTIFY_CHANGE_LAST_WRITE, FILE_NOTIFY_CHANGE_SIZE,
        FILE_NOTIFY_INFORMATION, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Threading::{CreateEventW, WaitForMultipleObjects, INFINITE};
    use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

    /// 中身の変更として見る通知
    const CONTENT_FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME
        | FILE_NOTIFY_CHANGE_DIR_NAME
        | FILE_NOTIFY_CHANGE_ATTRIBUTES
        | FILE_NOTIFY_CHANGE_SIZE
        | FILE_NOTIFY_CHANGE_LAST_WRITE
        | FILE_NOTIFY_CHANGE_CREATION;

    /// 1 回の読み取りの領域（`FILE_NOTIFY_INFORMATION` は 4 バイト境界に置く）
    const BUF_WORDS: usize = 8192;

    /// 1 つの `ReadDirectoryChangesW` の待ち（`OVERLAPPED` と領域は I/O の間に動かさない）
    struct Pending {
        handle: HANDLE,
        ov: Box<OVERLAPPED>,
        buf: Box<[u32; BUF_WORDS]>,
        subtree: bool,
        filter: u32,
        issued: bool,
    }

    // SAFETY: HANDLE は監視スレッドへ渡した後はそのスレッドだけが使う
    unsafe impl Send for Pending {}

    fn wide(p: &Path) -> Vec<u16> {
        p.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    fn open_dir(p: &Path) -> io::Result<HANDLE> {
        let w = wide(p);
        // SAFETY: w は NUL 終端の UTF-16。ディレクトリは FILE_FLAG_BACKUP_SEMANTICS で開く。
        // 共有に FILE_SHARE_DELETE を入れて、監視中もルートの改名・削除を妨げない
        let h = unsafe {
            CreateFileW(
                w.as_ptr(),
                FILE_LIST_DIRECTORY,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OVERLAPPED,
                std::ptr::null_mut(),
            )
        };
        if h == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        Ok(h)
    }

    impl Pending {
        fn new(handle: HANDLE, subtree: bool, filter: u32) -> io::Result<Self> {
            // SAFETY: 手動リセットのイベント（I/O の開始で非シグナルに戻る）
            let ev = unsafe { CreateEventW(std::ptr::null(), 1, 0, std::ptr::null()) };
            if ev.is_null() {
                let e = io::Error::last_os_error();
                // SAFETY: 開いたハンドル
                unsafe { CloseHandle(handle) };
                return Err(e);
            }
            // SAFETY: OVERLAPPED は POD
            let mut ov: Box<OVERLAPPED> = Box::new(unsafe { std::mem::zeroed() });
            ov.hEvent = ev;
            Ok(Self {
                handle,
                ov,
                buf: Box::new([0u32; BUF_WORDS]),
                subtree,
                filter,
                issued: false,
            })
        }

        fn issue(&mut self) -> io::Result<()> {
            // SAFETY: buf・ov は self が持ち、完了（または取り消しの完了）まで動かさない
            let ok = unsafe {
                ReadDirectoryChangesW(
                    self.handle,
                    self.buf.as_mut_ptr().cast(),
                    (BUF_WORDS * 4) as u32,
                    i32::from(self.subtree),
                    self.filter,
                    std::ptr::null_mut(),
                    &mut *self.ov,
                    None,
                )
            };
            // SAFETY: 引数なし
            if ok == 0 && unsafe { GetLastError() } != ERROR_IO_PENDING {
                self.issued = false;
                return Err(io::Error::last_os_error());
            }
            self.issued = true;
            Ok(())
        }

        /// 完了した読み取りのバイト数（0 は溢れ）
        fn complete(&mut self) -> io::Result<u32> {
            self.issued = false;
            let mut n = 0u32;
            // SAFETY: 完了した（イベントがシグナルの）I/O の結果を読む
            if unsafe { GetOverlappedResult(self.handle, &*self.ov, &mut n, 0) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(n)
        }

        /// 読んだ領域の名前と動作の列
        fn entries(&self, n: u32) -> Vec<(u32, OsString)> {
            let bytes = (n as usize).min(BUF_WORDS * 4);
            let base = self.buf.as_ptr().cast::<u8>();
            let head = std::mem::offset_of!(FILE_NOTIFY_INFORMATION, FileName);
            let mut out = Vec::new();
            let mut off = 0usize;
            while off + head <= bytes {
                // SAFETY: off + head <= bytes を確かめた。境界を仮定しない読み取り
                let info: FILE_NOTIFY_INFORMATION =
                    unsafe { std::ptr::read_unaligned(base.add(off).cast()) };
                let len =
                    (info.FileNameLength as usize / 2).min((bytes.saturating_sub(off + head)) / 2);
                let mut name = vec![0u16; len];
                // SAFETY: 名前は先頭から len 個の u16（上で領域内に収めた）
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        base.add(off + head),
                        name.as_mut_ptr().cast::<u8>(),
                        len * 2,
                    )
                };
                out.push((info.Action, OsString::from_wide(&name)));
                if info.NextEntryOffset == 0 {
                    break;
                }
                off += info.NextEntryOffset as usize;
            }
            out
        }

        /// 待っている I/O を取り消し、終わるのを待ってからハンドルを閉じる（イベントは残す）
        fn close_handle(&mut self) {
            if self.issued {
                let mut n = 0u32;
                // SAFETY: self.handle の self.ov の I/O を取り消し、完了を待つ（領域を手放す前に）
                unsafe {
                    CancelIoEx(self.handle, &*self.ov);
                    GetOverlappedResult(self.handle, &*self.ov, &mut n, 1);
                }
                self.issued = false;
            }
            if !self.handle.is_null() && self.handle != INVALID_HANDLE_VALUE {
                // SAFETY: 開いたハンドル
                unsafe { CloseHandle(self.handle) };
            }
            self.handle = INVALID_HANDLE_VALUE;
        }
    }

    impl Drop for Pending {
        fn drop(&mut self) {
            self.close_handle();
            // SAFETY: CreateEventW のイベント
            unsafe { CloseHandle(self.ov.hEvent) };
        }
    }

    /// ルートを監視するスレッドを立てる（最初の読み取りを始められたかを待って返す）
    pub(super) fn watch_root(path: &Path, gen: WatchGen) -> io::Result<()> {
        // 理由付き allow: 設定ロード時に呼ぶ（データプレーンの外）
        #[allow(clippy::disallowed_methods)]
        let is_dir = std::fs::metadata(path)?.is_dir();
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => PathBuf::from("."),
        };
        // 1 つのファイルのルートは親ディレクトリの中を見る（配下は辿らない）
        let (content_dir, subtree) = if is_dir {
            (path.to_path_buf(), true)
        } else {
            (parent.clone(), false)
        };
        let mut content = Pending::new(open_dir(&content_dir)?, subtree, CONTENT_FILTER)?;
        content.issue()?;
        // ルートの差し替えを見る親（開けなくても配下の監視は効くので失敗は無視する）
        let parent_watch = if is_dir {
            path.file_name().and_then(|name| {
                let h = open_dir(&parent).ok()?;
                let mut p = Pending::new(
                    h,
                    false,
                    FILE_NOTIFY_CHANGE_DIR_NAME | FILE_NOTIFY_CHANGE_FILE_NAME,
                )
                .ok()?;
                p.issue().ok()?;
                Some((p, name.to_string_lossy().to_lowercase()))
            })
        } else {
            None
        };
        let root = path.to_path_buf();
        std::thread::Builder::new()
            .name("veil-fswatch".into())
            .spawn(move || run(root, gen, content, parent_watch))?;
        Ok(())
    }

    fn run(
        root: PathBuf,
        gen: WatchGen,
        mut content: Pending,
        mut parent: Option<(Pending, String)>,
    ) {
        loop {
            let mut events: Vec<HANDLE> = Vec::with_capacity(2);
            if content.issued {
                events.push(content.ov.hEvent);
            }
            if let Some((p, _)) = parent.as_ref().filter(|(p, _)| p.issued) {
                events.push(p.ov.hEvent);
            }
            if events.is_empty() {
                ftlog::warn!(
                    "Static file watch for {} stopped; cached files follow disk changes only after valid_duration_secs",
                    root.display()
                );
                return;
            }
            // SAFETY: events は有効なイベントのハンドル
            let w = unsafe {
                WaitForMultipleObjects(events.len() as u32, events.as_ptr(), 0, INFINITE)
            };
            let which = w.wrapping_sub(WAIT_OBJECT_0) as usize;
            if which >= events.len() {
                ftlog::warn!(
                    "Static file watch for {} stopped ({}); cached files follow disk changes only after valid_duration_secs",
                    root.display(),
                    io::Error::last_os_error()
                );
                return;
            }
            if events[which] == content.ov.hEvent {
                // 何が変わったかは見ない（溢れた 0 バイトの完了も含めて世代を増やす）
                let _ = content.complete();
                gen.bump();
                if content.issue().is_err() {
                    // ルートが消えた: 親が新しいルートを見つけたら開き直す
                    content.close_handle();
                }
                continue;
            }
            let Some((p, want)) = parent.as_mut() else {
                continue;
            };
            let n = p.complete().unwrap_or(0);
            let swapped = n == 0
                || p.entries(n).iter().any(|(action, name)| {
                    (*action == FILE_ACTION_ADDED || *action == FILE_ACTION_RENAMED_NEW_NAME)
                        && name.to_string_lossy().to_lowercase() == *want
                });
            if p.issue().is_err() {
                p.close_handle();
            }
            if !swapped {
                continue;
            }
            // ルートが差し替わった（かもしれない）: 中身の監視を開き直す
            gen.bump();
            content.close_handle();
            match open_dir(&root) {
                Ok(h) => {
                    content.handle = h;
                    if let Err(e) = content.issue() {
                        content.close_handle();
                        super::warn_root(&root, &e);
                    }
                }
                Err(e) => super::warn_root(&root, &e),
            }
            gen.bump();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaked() -> WatchGen {
        WatchGen(Box::leak(Box::new(AtomicU64::new(7))))
    }

    #[test]
    fn stamp_is_fresh_until_bumped() {
        let g = leaked();
        let s = stamp(Some(g));
        assert!(fresh(s, Some(g)));
        // 監視しないルートの要求からも、作った後に変わっていなければ使える
        assert!(fresh(s, None));
        g.bump();
        assert!(!fresh(s, Some(g)));
        assert!(!fresh(s, None));
        // 作り直した（新しい世代を読んだ）エントリは使える
        assert!(fresh(stamp(Some(g)), Some(g)));
    }

    #[test]
    fn unstamped_entry_is_only_for_unwatched_routes() {
        let g = leaked();
        assert!(fresh(None, None));
        assert!(!fresh(None, Some(g)));
    }

    #[test]
    fn bump_all_invalidates_every_root() {
        let a = leaked();
        let b = leaked();
        let st = State {
            roots: vec![
                RootEntry {
                    path: PathBuf::from("/a"),
                    gen: a,
                    active: true,
                },
                RootEntry {
                    path: PathBuf::from("/b"),
                    gen: b,
                    active: true,
                },
            ],
            #[cfg(target_os = "linux")]
            inotify: None,
            #[cfg(any(
                target_os = "freebsd",
                target_os = "openbsd",
                target_os = "netbsd",
                target_os = "macos"
            ))]
            kq: None,
        };
        let (sa, sb) = (a.stamp(), b.stamp());
        bump_all(&st);
        assert!(!fresh(Some(sa), Some(a)));
        assert!(!fresh(Some(sb), Some(b)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_reads_names_and_overflow_bumps_all() {
        let mut raw = linux::encode(3, libc::IN_MOVED_TO, b"index.html");
        raw.extend(linux::encode(-1, libc::IN_Q_OVERFLOW, b""));
        let evs = linux::parse(&raw);
        assert_eq!(evs.len(), 2);
        assert_eq!((evs[0].wd, evs[0].name), (3, &b"index.html"[..]));
        assert_eq!(evs[1].mask, libc::IN_Q_OVERFLOW);

        let a = leaked();
        let mut st = State {
            roots: vec![RootEntry {
                path: PathBuf::from("/a"),
                gen: a,
                active: true,
            }],
            inotify: None,
        };
        let before = a.current();
        for ev in evs {
            linux::handle(&mut st, ev);
        }
        assert_eq!(a.current(), before + 1);
    }

    /// 実際の通知（inotify・kqueue・ReadDirectoryChangesW）: 同じサイズの `mv` の差し替え・中身の上書き・
    /// 後から作ったディレクトリ・ルートの差し替え
    #[cfg(any(
        target_os = "linux",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "macos",
        windows
    ))]
    #[test]
    #[allow(clippy::disallowed_methods)]
    fn watcher_bumps_on_replace_overwrite_new_dir_and_root_swap() {
        use std::time::{Duration, Instant};
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("www");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.txt"), b"AAAA").unwrap();
        let Some(g) = register(&root) else {
            // 通知が使えない環境（監視の上限が 0 等）では試さない
            eprintln!("file change notification unavailable; skipping");
            return;
        };
        // 同じパスは同じ世代
        assert!(std::ptr::eq(register(&root).unwrap().0, g.0));

        let wait_bump = |s: Stamp, what: &str| {
            let t = Instant::now();
            while fresh(Some(s), Some(g)) {
                assert!(
                    t.elapsed() < Duration::from_secs(5),
                    "no notification for {what}"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        };

        let s = g.stamp();
        std::fs::write(base.path().join("tmp.txt"), b"BBBB").unwrap();
        std::fs::rename(base.path().join("tmp.txt"), root.join("a.txt")).unwrap();
        wait_bump(s, "same-size mv replacement");

        let s = g.stamp();
        std::fs::write(root.join("a.txt"), b"CCCC").unwrap();
        wait_bump(s, "in-place overwrite");

        let s = g.stamp();
        std::fs::create_dir(root.join("sub")).unwrap();
        wait_bump(s, "new directory");
        // 新しいディレクトリの中の変更も届く（付け足しが済むまで少し待つ）
        std::thread::sleep(Duration::from_millis(100));
        let s = g.stamp();
        std::fs::write(root.join("sub").join("b.txt"), b"x").unwrap();
        wait_bump(s, "file in a new directory");

        // ルートのディレクトリそのものの差し替え
        let next = base.path().join("www.next");
        std::fs::create_dir(&next).unwrap();
        std::fs::write(next.join("a.txt"), b"DDDD").unwrap();
        let s = g.stamp();
        std::fs::rename(&root, base.path().join("www.old")).unwrap();
        std::fs::rename(&next, &root).unwrap();
        wait_bump(s, "root swap");
        std::thread::sleep(Duration::from_millis(100));
        let s = g.stamp();
        std::fs::write(root.join("a.txt"), b"EEEE").unwrap();
        wait_bump(s, "change in the swapped root");
    }
}
