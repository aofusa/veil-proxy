//! `tokio::io` 互換（F-198）。トレイト（`AsyncRead`・`AsyncWrite`・`AsyncBufRead`・`AsyncSeek`）は tokio と同じ
//! poll ベースの定義で、`ReadBuf` も同じ。拡張トレイト・バッファ・分割・`duplex`・`copy` はランタイムに依存しない。

mod read_buf;
mod util;

pub use read_buf::ReadBuf;
pub use std::io::{Error, ErrorKind, Result, SeekFrom};
pub use util::*;

use std::ops::DerefMut;
use std::pin::Pin;
use std::task::{Context, Poll};

/// `tokio::io::AsyncRead`。
pub trait AsyncRead {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>>;
}

/// `tokio::io::AsyncWrite`。
pub trait AsyncWrite {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize>>;

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>>;

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>>;

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<Result<usize>> {
        let buf = bufs
            .iter()
            .find(|b| !b.is_empty())
            .map_or(&[][..], |b| &**b);
        self.poll_write(cx, buf)
    }

    fn is_write_vectored(&self) -> bool {
        false
    }
}

/// `tokio::io::AsyncBufRead`。
pub trait AsyncBufRead: AsyncRead {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<&[u8]>>;
    fn consume(self: Pin<&mut Self>, amt: usize);
}

/// `tokio::io::AsyncSeek`。
pub trait AsyncSeek {
    fn start_seek(self: Pin<&mut Self>, position: SeekFrom) -> Result<()>;
    fn poll_complete(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<u64>>;
}

// ===================== 参照・Box・Pin への委譲 =====================

macro_rules! deref_async_read {
    () => {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<Result<()>> {
            Pin::new(&mut **self).poll_read(cx, buf)
        }
    };
}

impl<T: ?Sized + AsyncRead + Unpin> AsyncRead for Box<T> {
    deref_async_read!();
}

impl<T: ?Sized + AsyncRead + Unpin> AsyncRead for &mut T {
    deref_async_read!();
}

impl<P> AsyncRead for Pin<P>
where
    P: DerefMut + Unpin,
    P::Target: AsyncRead,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        self.get_mut().as_mut().poll_read(cx, buf)
    }
}

impl AsyncRead for &[u8] {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        let n = std::cmp::min(self.len(), buf.remaining());
        let (a, b) = self.split_at(n);
        buf.put_slice(a);
        *self = b;
        Poll::Ready(Ok(()))
    }
}

impl<T: AsRef<[u8]> + Unpin> AsyncRead for std::io::Cursor<T> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        let pos = self.position();
        let slice: &[u8] = (*self).get_ref().as_ref();
        if pos > slice.len() as u64 {
            return Poll::Ready(Ok(()));
        }
        let start = pos as usize;
        let amt = std::cmp::min(slice.len() - start, buf.remaining());
        buf.put_slice(&slice[start..start + amt]);
        self.set_position(pos + amt as u64);
        Poll::Ready(Ok(()))
    }
}

macro_rules! deref_async_write {
    () => {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<Result<usize>> {
            Pin::new(&mut **self).poll_write(cx, buf)
        }

        fn poll_write_vectored(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bufs: &[std::io::IoSlice<'_>],
        ) -> Poll<Result<usize>> {
            Pin::new(&mut **self).poll_write_vectored(cx, bufs)
        }

        fn is_write_vectored(&self) -> bool {
            (**self).is_write_vectored()
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
            Pin::new(&mut **self).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
            Pin::new(&mut **self).poll_shutdown(cx)
        }
    };
}

impl<T: ?Sized + AsyncWrite + Unpin> AsyncWrite for Box<T> {
    deref_async_write!();
}

impl<T: ?Sized + AsyncWrite + Unpin> AsyncWrite for &mut T {
    deref_async_write!();
}

impl<P> AsyncWrite for Pin<P>
where
    P: DerefMut + Unpin,
    P::Target: AsyncWrite,
{
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize>> {
        self.get_mut().as_mut().poll_write(cx, buf)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<Result<usize>> {
        self.get_mut().as_mut().poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        (**self).is_write_vectored()
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        self.get_mut().as_mut().poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        self.get_mut().as_mut().poll_shutdown(cx)
    }
}

impl AsyncWrite for Vec<u8> {
    fn poll_write(self: Pin<&mut Self>, _cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize>> {
        self.get_mut().extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<Result<usize>> {
        Poll::Ready(std::io::Write::write_vectored(self.get_mut(), bufs))
    }

    fn is_write_vectored(&self) -> bool {
        true
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
        Poll::Ready(Ok(()))
    }
}

macro_rules! cursor_write {
    ($($t:ty),*) => {
        $(
            impl AsyncWrite for std::io::Cursor<$t> {
                fn poll_write(
                    self: Pin<&mut Self>,
                    _cx: &mut Context<'_>,
                    buf: &[u8],
                ) -> Poll<Result<usize>> {
                    Poll::Ready(std::io::Write::write(self.get_mut(), buf))
                }

                fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<()>> {
                    Poll::Ready(std::io::Write::flush(self.get_mut()))
                }

                fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
                    self.poll_flush(cx)
                }
            }
        )*
    };
}

cursor_write!(&mut [u8], &mut Vec<u8>, Vec<u8>, Box<[u8]>);

impl<T: ?Sized + AsyncBufRead + Unpin> AsyncBufRead for Box<T> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<&[u8]>> {
        Pin::new(&mut **self.get_mut()).poll_fill_buf(cx)
    }
    fn consume(self: Pin<&mut Self>, amt: usize) {
        Pin::new(&mut **self.get_mut()).consume(amt)
    }
}

impl<T: ?Sized + AsyncBufRead + Unpin> AsyncBufRead for &mut T {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<&[u8]>> {
        Pin::new(&mut **self.get_mut()).poll_fill_buf(cx)
    }
    fn consume(self: Pin<&mut Self>, amt: usize) {
        Pin::new(&mut **self.get_mut()).consume(amt)
    }
}

impl<P> AsyncBufRead for Pin<P>
where
    P: DerefMut + Unpin,
    P::Target: AsyncBufRead,
{
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<&[u8]>> {
        self.get_mut().as_mut().poll_fill_buf(cx)
    }
    fn consume(self: Pin<&mut Self>, amt: usize) {
        self.get_mut().as_mut().consume(amt)
    }
}

impl AsyncBufRead for &[u8] {
    fn poll_fill_buf(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<&[u8]>> {
        Poll::Ready(Ok(*self.get_mut()))
    }
    fn consume(mut self: Pin<&mut Self>, amt: usize) {
        *self = &self[amt..];
    }
}

impl<T: AsRef<[u8]> + Unpin> AsyncBufRead for std::io::Cursor<T> {
    fn poll_fill_buf(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<&[u8]>> {
        Poll::Ready(std::io::BufRead::fill_buf(self.get_mut()))
    }
    fn consume(self: Pin<&mut Self>, amt: usize) {
        std::io::BufRead::consume(self.get_mut(), amt)
    }
}

impl<T: AsRef<[u8]> + Unpin> AsyncSeek for std::io::Cursor<T> {
    fn start_seek(self: Pin<&mut Self>, pos: SeekFrom) -> Result<()> {
        std::io::Seek::seek(self.get_mut(), pos).map(drop)
    }
    fn poll_complete(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<u64>> {
        Poll::Ready(Ok(self.get_mut().position()))
    }
}

impl<T: ?Sized + AsyncSeek + Unpin> AsyncSeek for Box<T> {
    fn start_seek(self: Pin<&mut Self>, pos: SeekFrom) -> Result<()> {
        Pin::new(&mut **self.get_mut()).start_seek(pos)
    }
    fn poll_complete(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<u64>> {
        Pin::new(&mut **self.get_mut()).poll_complete(cx)
    }
}

impl<T: ?Sized + AsyncSeek + Unpin> AsyncSeek for &mut T {
    fn start_seek(self: Pin<&mut Self>, pos: SeekFrom) -> Result<()> {
        Pin::new(&mut **self.get_mut()).start_seek(pos)
    }
    fn poll_complete(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<u64>> {
        Pin::new(&mut **self.get_mut()).poll_complete(cx)
    }
}

// ===================== 準備状態（net の readiness API） =====================

/// 待つ向き（`tokio::io::Interest`）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Interest(u8);

impl Interest {
    pub const READABLE: Interest = Interest(0b01);
    pub const WRITABLE: Interest = Interest(0b10);
    pub const ERROR: Interest = Interest(0b100);
    pub const PRIORITY: Interest = Interest(0b1000);

    pub const fn is_readable(self) -> bool {
        self.0 & 0b01 != 0
    }
    pub const fn is_writable(self) -> bool {
        self.0 & 0b10 != 0
    }
    pub const fn is_error(self) -> bool {
        self.0 & 0b100 != 0
    }
    pub const fn is_priority(self) -> bool {
        self.0 & 0b1000 != 0
    }
    pub const fn add(self, other: Interest) -> Interest {
        Interest(self.0 | other.0)
    }
    pub fn remove(self, other: Interest) -> Option<Interest> {
        let v = self.0 & !other.0;
        (v != 0).then_some(Interest(v))
    }
}

impl std::ops::BitOr for Interest {
    type Output = Interest;
    fn bitor(self, o: Interest) -> Interest {
        self.add(o)
    }
}

impl std::ops::BitOrAssign for Interest {
    fn bitor_assign(&mut self, o: Interest) {
        *self = self.add(o);
    }
}

/// 準備できた向き（`tokio::io::Ready`）。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Ready(u8);

impl Ready {
    pub const EMPTY: Ready = Ready(0);
    pub const READABLE: Ready = Ready(0b1);
    pub const WRITABLE: Ready = Ready(0b10);
    pub const READ_CLOSED: Ready = Ready(0b100);
    pub const WRITE_CLOSED: Ready = Ready(0b1000);
    pub const ERROR: Ready = Ready(0b10000);
    pub const PRIORITY: Ready = Ready(0b100000);
    pub const ALL: Ready = Ready(0b111111);

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
    pub fn is_readable(self) -> bool {
        self.0 & (0b1 | 0b100) != 0
    }
    pub fn is_writable(self) -> bool {
        self.0 & (0b10 | 0b1000) != 0
    }
    pub fn is_read_closed(self) -> bool {
        self.0 & 0b100 != 0
    }
    pub fn is_write_closed(self) -> bool {
        self.0 & 0b1000 != 0
    }
    pub fn is_error(self) -> bool {
        self.0 & 0b10000 != 0
    }
    pub fn is_priority(self) -> bool {
        self.0 & 0b100000 != 0
    }
}

impl std::ops::BitOr for Ready {
    type Output = Ready;
    fn bitor(self, o: Ready) -> Ready {
        Ready(self.0 | o.0)
    }
}

impl std::ops::BitOrAssign for Ready {
    fn bitor_assign(&mut self, o: Ready) {
        self.0 |= o.0;
    }
}
