//! `tokio::fs` 互換（F-198）。同期のファイル操作をブロッキング用プール（`task::spawn_blocking`）で行う。
// 理由付き allow: このモジュールの std::fs 呼び出しはすべて spawn_blocking の中（ワーカーのスレッドでは動かない）。
#![allow(clippy::disallowed_methods)]

use super::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};
use super::task::{spawn_blocking, JoinHandle};
use std::future::Future;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{ready, Context, Poll};

async fn asyncify<F, T>(f: F) -> io::Result<T>
where
    F: FnOnce() -> io::Result<T> + Send + 'static,
    T: Send + 'static,
{
    match spawn_blocking(f).await {
        Ok(r) => r,
        Err(_) => Err(io::Error::other("background task failed")),
    }
}

pub async fn read(path: impl AsRef<Path>) -> io::Result<Vec<u8>> {
    let p = path.as_ref().to_owned();
    asyncify(move || std::fs::read(p)).await
}

pub async fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    let p = path.as_ref().to_owned();
    asyncify(move || std::fs::read_to_string(p)).await
}

pub async fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) -> io::Result<()> {
    let p = path.as_ref().to_owned();
    let c = contents.as_ref().to_owned();
    asyncify(move || std::fs::write(p, c)).await
}

pub async fn metadata(path: impl AsRef<Path>) -> io::Result<std::fs::Metadata> {
    let p = path.as_ref().to_owned();
    asyncify(move || std::fs::metadata(p)).await
}

pub async fn symlink_metadata(path: impl AsRef<Path>) -> io::Result<std::fs::Metadata> {
    let p = path.as_ref().to_owned();
    asyncify(move || std::fs::symlink_metadata(p)).await
}

pub async fn try_exists(path: impl AsRef<Path>) -> io::Result<bool> {
    let p = path.as_ref().to_owned();
    asyncify(move || p.try_exists()).await
}

pub async fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    let p = path.as_ref().to_owned();
    asyncify(move || std::fs::canonicalize(p)).await
}

pub async fn create_dir(path: impl AsRef<Path>) -> io::Result<()> {
    let p = path.as_ref().to_owned();
    asyncify(move || std::fs::create_dir(p)).await
}

pub async fn create_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    let p = path.as_ref().to_owned();
    asyncify(move || std::fs::create_dir_all(p)).await
}

pub async fn remove_file(path: impl AsRef<Path>) -> io::Result<()> {
    let p = path.as_ref().to_owned();
    asyncify(move || std::fs::remove_file(p)).await
}

pub async fn remove_dir(path: impl AsRef<Path>) -> io::Result<()> {
    let p = path.as_ref().to_owned();
    asyncify(move || std::fs::remove_dir(p)).await
}

pub async fn remove_dir_all(path: impl AsRef<Path>) -> io::Result<()> {
    let p = path.as_ref().to_owned();
    asyncify(move || std::fs::remove_dir_all(p)).await
}

pub async fn rename(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<()> {
    let (f, t) = (from.as_ref().to_owned(), to.as_ref().to_owned());
    asyncify(move || std::fs::rename(f, t)).await
}

pub async fn copy(from: impl AsRef<Path>, to: impl AsRef<Path>) -> io::Result<u64> {
    let (f, t) = (from.as_ref().to_owned(), to.as_ref().to_owned());
    asyncify(move || std::fs::copy(f, t)).await
}

pub async fn hard_link(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> io::Result<()> {
    let (s, d) = (src.as_ref().to_owned(), dst.as_ref().to_owned());
    asyncify(move || std::fs::hard_link(s, d)).await
}

#[cfg(unix)]
pub async fn symlink(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> io::Result<()> {
    let (s, d) = (src.as_ref().to_owned(), dst.as_ref().to_owned());
    asyncify(move || std::os::unix::fs::symlink(s, d)).await
}

pub async fn read_link(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    let p = path.as_ref().to_owned();
    asyncify(move || std::fs::read_link(p)).await
}

pub async fn set_permissions(path: impl AsRef<Path>, perm: std::fs::Permissions) -> io::Result<()> {
    let p = path.as_ref().to_owned();
    asyncify(move || std::fs::set_permissions(p, perm)).await
}

/// `tokio::fs::read_dir` の結果。
#[derive(Debug)]
pub struct ReadDir {
    entries: std::vec::IntoIter<io::Result<std::fs::DirEntry>>,
}

/// ディレクトリの 1 項目。
#[derive(Debug)]
pub struct DirEntry(std::fs::DirEntry);

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.0.path()
    }
    pub fn file_name(&self) -> std::ffi::OsString {
        self.0.file_name()
    }
    pub async fn metadata(&self) -> io::Result<std::fs::Metadata> {
        metadata(self.0.path()).await
    }
    pub async fn file_type(&self) -> io::Result<std::fs::FileType> {
        Ok(symlink_metadata(self.0.path()).await?.file_type())
    }
}

impl ReadDir {
    pub fn poll_next_entry(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<Option<DirEntry>>> {
        Poll::Ready(match self.entries.next() {
            Some(Ok(e)) => Ok(Some(DirEntry(e))),
            Some(Err(e)) => Err(e),
            None => Ok(None),
        })
    }

    pub async fn next_entry(&mut self) -> io::Result<Option<DirEntry>> {
        match self.entries.next() {
            Some(Ok(e)) => Ok(Some(DirEntry(e))),
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }
}

pub async fn read_dir(path: impl AsRef<Path>) -> io::Result<ReadDir> {
    let p = path.as_ref().to_owned();
    let entries = asyncify(move || Ok(std::fs::read_dir(p)?.collect::<Vec<_>>())).await?;
    Ok(ReadDir {
        entries: entries.into_iter(),
    })
}

// ===================== File =====================

enum Op {
    Read(io::Result<Vec<u8>>),
    Write(io::Result<usize>),
    Seek(io::Result<u64>),
}

/// `tokio::fs::File`（読み書きは 1 回ずつブロッキング用プールで行う）。
pub struct File {
    std: Arc<Mutex<std::fs::File>>,
    busy: Option<JoinHandle<Op>>,
    /// 読み込み済みでまだ渡していない分
    buf: Vec<u8>,
    pos: usize,
    last_write_err: Option<io::Error>,
    max_buf_size: usize,
}

impl std::fmt::Debug for File {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("tokio::fs::File")
    }
}

impl File {
    pub async fn open(path: impl AsRef<Path>) -> io::Result<File> {
        let p = path.as_ref().to_owned();
        let f = asyncify(move || std::fs::File::open(p)).await?;
        Ok(File::from_std(f))
    }

    pub async fn create(path: impl AsRef<Path>) -> io::Result<File> {
        let p = path.as_ref().to_owned();
        let f = asyncify(move || std::fs::File::create(p)).await?;
        Ok(File::from_std(f))
    }

    pub async fn create_new(path: impl AsRef<Path>) -> io::Result<File> {
        let p = path.as_ref().to_owned();
        let f = asyncify(move || std::fs::File::create_new(p)).await?;
        Ok(File::from_std(f))
    }

    pub fn options() -> OpenOptions {
        OpenOptions::new()
    }

    pub fn from_std(std: std::fs::File) -> File {
        File {
            std: Arc::new(Mutex::new(std)),
            busy: None,
            buf: Vec::new(),
            pos: 0,
            last_write_err: None,
            max_buf_size: 2 * 1024 * 1024,
        }
    }

    pub async fn into_std(mut self) -> std::fs::File {
        if let Some(h) = self.busy.take() {
            let _ = h.await;
        }
        match Arc::try_unwrap(self.std) {
            Ok(m) => m.into_inner().unwrap(),
            Err(a) => a.lock().unwrap().try_clone().expect("try_clone"),
        }
    }

    pub fn try_into_std(self) -> Result<std::fs::File, File> {
        if self.busy.is_some() {
            return Err(self);
        }
        match Arc::try_unwrap(self.std) {
            Ok(m) => Ok(m.into_inner().unwrap()),
            Err(a) => Err(File {
                std: a,
                busy: None,
                buf: Vec::new(),
                pos: 0,
                last_write_err: None,
                max_buf_size: 2 * 1024 * 1024,
            }),
        }
    }

    async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut std::fs::File) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        let std = self.std.clone();
        asyncify(move || f(&mut std.lock().unwrap())).await
    }

    /// 1 回の読み書きでブロッキング用プールへ渡す最大のバイト数。
    pub fn set_max_buf_size(&mut self, max_buf_size: usize) {
        self.max_buf_size = max_buf_size.max(1);
    }

    pub fn max_buf_size(&self) -> usize {
        self.max_buf_size
    }

    pub async fn sync_all(&mut self) -> io::Result<()> {
        self.flush_busy().await?;
        self.run(|f| f.sync_all()).await
    }

    pub async fn sync_data(&mut self) -> io::Result<()> {
        self.flush_busy().await?;
        self.run(|f| f.sync_data()).await
    }

    pub async fn set_len(&mut self, size: u64) -> io::Result<()> {
        self.flush_busy().await?;
        self.run(move |f| f.set_len(size)).await
    }

    pub async fn metadata(&self) -> io::Result<std::fs::Metadata> {
        self.run(|f| f.metadata()).await
    }

    pub async fn try_clone(&self) -> io::Result<File> {
        Ok(File::from_std(self.run(|f| f.try_clone()).await?))
    }

    pub async fn set_permissions(&self, perm: std::fs::Permissions) -> io::Result<()> {
        self.run(move |f| f.set_permissions(perm)).await
    }

    async fn flush_busy(&mut self) -> io::Result<()> {
        std::future::poll_fn(|cx| self.poll_busy(cx).map(|r| r.map(drop))).await
    }

    /// 進行中の操作の完了を待つ。
    fn poll_busy(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<Option<Op>>> {
        let Some(h) = self.busy.as_mut() else {
            return Poll::Ready(Ok(None));
        };
        let r = ready!(Pin::new(h).poll(cx));
        self.busy = None;
        match r {
            Ok(Op::Write(Err(e))) => {
                self.last_write_err = Some(e);
                Poll::Ready(Ok(None))
            }
            Ok(op) => Poll::Ready(Ok(Some(op))),
            Err(_) => Poll::Ready(Err(io::Error::other("background task failed"))),
        }
    }

    fn start(&mut self, f: impl FnOnce(&mut std::fs::File) -> Op + Send + 'static) {
        let std = self.std.clone();
        self.busy = Some(spawn_blocking(move || f(&mut std.lock().unwrap())));
    }
}

impl AsyncRead for File {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        dst: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let me = &mut *self;
        loop {
            if me.pos < me.buf.len() {
                let n = std::cmp::min(dst.remaining(), me.buf.len() - me.pos);
                dst.put_slice(&me.buf[me.pos..me.pos + n]);
                me.pos += n;
                return Poll::Ready(Ok(()));
            }
            if let Some(Op::Read(r)) = ready!(me.poll_busy(cx))? {
                me.buf = r?;
                me.pos = 0;
                if me.buf.is_empty() {
                    return Poll::Ready(Ok(()));
                }
                continue;
            }
            let want = dst.remaining().clamp(1, me.max_buf_size);
            me.start(move |f| {
                let mut b = vec![0u8; want];
                Op::Read(f.read(&mut b).map(|n| {
                    b.truncate(n);
                    b
                }))
            });
        }
    }
}

impl AsyncWrite for File {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        src: &[u8],
    ) -> Poll<io::Result<usize>> {
        let me = &mut *self;
        if let Some(e) = me.last_write_err.take() {
            return Poll::Ready(Err(e));
        }
        ready!(me.poll_busy(cx))?;
        // 読み込み済みで未消費の分があれば位置を戻す
        if me.pos < me.buf.len() {
            let back = (me.buf.len() - me.pos) as i64;
            me.buf.clear();
            me.pos = 0;
            me.start(move |f| Op::Seek(f.seek(SeekFrom::Current(-back))));
            ready!(me.poll_busy(cx))?;
        }
        let data = src[..src.len().min(me.max_buf_size)].to_vec();
        let n = data.len();
        me.start(move |f| Op::Write(f.write_all(&data).map(|_| data.len())));
        Poll::Ready(Ok(n))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let me = &mut *self;
        ready!(me.poll_busy(cx))?;
        if let Some(e) = me.last_write_err.take() {
            return Poll::Ready(Err(e));
        }
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.poll_flush(cx)
    }
}

impl AsyncSeek for File {
    fn start_seek(mut self: Pin<&mut Self>, pos: SeekFrom) -> io::Result<()> {
        let me = &mut *self;
        if me.busy.is_some() {
            return Err(io::Error::other(
                "other file operation is pending, call poll_complete before start_seek",
            ));
        }
        let pos = match pos {
            SeekFrom::Current(n) => SeekFrom::Current(n - (me.buf.len() - me.pos) as i64),
            p => p,
        };
        me.buf.clear();
        me.pos = 0;
        me.start(move |f| Op::Seek(f.seek(pos)));
        Ok(())
    }

    fn poll_complete(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        let me = &mut *self;
        match ready!(me.poll_busy(cx))? {
            Some(Op::Seek(r)) => Poll::Ready(r),
            _ => {
                me.start(|f| Op::Seek(f.stream_position()));
                match ready!(me.poll_busy(cx))? {
                    Some(Op::Seek(r)) => Poll::Ready(r),
                    _ => Poll::Ready(Err(io::Error::other("seek failed"))),
                }
            }
        }
    }
}

#[cfg(unix)]
impl std::os::fd::AsRawFd for File {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.std.lock().unwrap().as_raw_fd()
    }
}

#[cfg(unix)]
impl std::os::fd::FromRawFd for File {
    unsafe fn from_raw_fd(fd: std::os::fd::RawFd) -> File {
        // SAFETY: 呼び出し側が所有権を渡す
        File::from_std(unsafe { std::fs::File::from_raw_fd(fd) })
    }
}

impl From<std::fs::File> for File {
    fn from(f: std::fs::File) -> File {
        File::from_std(f)
    }
}

/// `tokio::fs::OpenOptions`。
#[derive(Clone, Debug)]
pub struct OpenOptions(std::fs::OpenOptions);

impl Default for OpenOptions {
    fn default() -> Self {
        OpenOptions::new()
    }
}

impl OpenOptions {
    pub fn new() -> OpenOptions {
        OpenOptions(std::fs::OpenOptions::new())
    }
    pub fn read(&mut self, v: bool) -> &mut Self {
        self.0.read(v);
        self
    }
    pub fn write(&mut self, v: bool) -> &mut Self {
        self.0.write(v);
        self
    }
    pub fn append(&mut self, v: bool) -> &mut Self {
        self.0.append(v);
        self
    }
    pub fn truncate(&mut self, v: bool) -> &mut Self {
        self.0.truncate(v);
        self
    }
    pub fn create(&mut self, v: bool) -> &mut Self {
        self.0.create(v);
        self
    }
    pub fn create_new(&mut self, v: bool) -> &mut Self {
        self.0.create_new(v);
        self
    }
    #[cfg(unix)]
    pub fn custom_flags(&mut self, flags: i32) -> &mut Self {
        std::os::unix::fs::OpenOptionsExt::custom_flags(&mut self.0, flags);
        self
    }

    #[cfg(unix)]
    pub fn mode(&mut self, mode: u32) -> &mut Self {
        std::os::unix::fs::OpenOptionsExt::mode(&mut self.0, mode);
        self
    }
    pub async fn open(&self, path: impl AsRef<Path>) -> io::Result<File> {
        let o = self.0.clone();
        let p = path.as_ref().to_owned();
        Ok(File::from_std(asyncify(move || o.open(p)).await?))
    }
}

/// `tokio::fs::DirBuilder`。
#[derive(Debug, Default)]
pub struct DirBuilder {
    recursive: bool,
    #[cfg(unix)]
    mode: Option<u32>,
}

impl DirBuilder {
    pub fn new() -> DirBuilder {
        DirBuilder::default()
    }

    pub fn recursive(&mut self, recursive: bool) -> &mut Self {
        self.recursive = recursive;
        self
    }

    #[cfg(unix)]
    pub fn mode(&mut self, mode: u32) -> &mut Self {
        self.mode = Some(mode);
        self
    }

    pub async fn create(&self, path: impl AsRef<Path>) -> io::Result<()> {
        let p = path.as_ref().to_owned();
        let mut b = std::fs::DirBuilder::new();
        b.recursive(self.recursive);
        #[cfg(unix)]
        if let Some(m) = self.mode {
            std::os::unix::fs::DirBuilderExt::mode(&mut b, m);
        }
        asyncify(move || b.create(p)).await
    }
}
