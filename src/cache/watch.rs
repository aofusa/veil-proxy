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
//! - それ以外の OS: まだ監視しない（警告して有効期限だけの動きに戻す）。
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

    /// テスト・診断用: 今の世代。
    #[cfg(test)]
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
}

static STATE: Mutex<State> = Mutex::new(State {
    roots: Vec::new(),
    #[cfg(target_os = "linux")]
    inotify: None,
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

/// 全部のルートの世代を増やす（通知が溢れたとき）。
fn bump_all(st: &State) {
    for r in &st.roots {
        r.gen.bump();
    }
}

#[cfg(target_os = "linux")]
fn platform_watch(st: &mut State, idx: usize) -> std::io::Result<()> {
    linux::watch_root(st, idx)
}

#[cfg(not(target_os = "linux"))]
fn platform_watch(_st: &mut State, _idx: usize) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "file change notification is not implemented on this platform",
    ))
}

#[cfg(target_os = "linux")]
mod linux {
    //! inotify の監視（専用スレッドがブロッキングの `read` で受ける。io_uring は使わない）。
    use super::{bump_all, lock_state, State};
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
                return Err(io::Error::last_os_error());
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
                return Err(io::Error::last_os_error());
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

    fn warn_partial(st: &State, root: usize, e: &io::Error) {
        let path = st
            .roots
            .get(root)
            .map(|r| r.path.display().to_string())
            .unwrap_or_default();
        ftlog::warn!(
            "Static file watch for {} could not follow a new directory ({}); changes there are picked up after valid_duration_secs",
            path,
            e
        );
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

    /// 実際の inotify: 同じサイズの `mv` の差し替え・中身の上書き・後から作ったディレクトリ・ルートの差し替え
    #[cfg(target_os = "linux")]
    #[test]
    #[allow(clippy::disallowed_methods)]
    fn inotify_bumps_on_replace_overwrite_new_dir_and_root_swap() {
        use std::time::{Duration, Instant};
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("www");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("a.txt"), b"AAAA").unwrap();
        let Some(g) = register(&root) else {
            // inotify が使えない環境（監視の上限が 0 等）では試さない
            eprintln!("inotify unavailable; skipping");
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
