//! Utilities of `tokio::io`: extension traits, buffers, splitting, `duplex`, `copy` and standard I/O.

use super::{AsyncBufRead, AsyncRead, AsyncSeek, AsyncWrite, ReadBuf, Result, SeekFrom};
use crate::tokio::waker::WakerSlot;
use std::future::{poll_fn, Future};
use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{ready, Context, Poll};

// ===================== AsyncReadExt =====================

macro_rules! read_num {
    ($($name:ident, $t:ty, $from:ident;)*) => {
        $(
            #[doc = concat!("`", stringify!($t), "` from the stream.")]
            fn $name(&mut self) -> impl Future<Output = Result<$t>> + '_
            where
                Self: Unpin,
            {
                async move {
                    let mut b = [0u8; std::mem::size_of::<$t>()];
                    self.read_exact(&mut b).await?;
                    Ok(<$t>::$from(b))
                }
            }
        )*
    };
}

/// `tokio::io::AsyncReadExt`。
pub trait AsyncReadExt: AsyncRead {
    fn read<'a>(&'a mut self, buf: &'a mut [u8]) -> impl Future<Output = Result<usize>> + 'a
    where
        Self: Unpin,
    {
        poll_fn(move |cx| {
            let mut rb = ReadBuf::new(&mut *buf);
            ready!(Pin::new(&mut *self).poll_read(cx, &mut rb))?;
            Poll::Ready(Ok(rb.filled().len()))
        })
    }

    fn read_buf<'a, B: bytes::BufMut + ?Sized>(
        &'a mut self,
        buf: &'a mut B,
    ) -> impl Future<Output = Result<usize>> + 'a
    where
        Self: Unpin,
    {
        poll_fn(move |cx| poll_read_buf(Pin::new(&mut *self), cx, &mut *buf))
    }

    fn read_exact<'a>(&'a mut self, buf: &'a mut [u8]) -> impl Future<Output = Result<usize>> + 'a
    where
        Self: Unpin,
    {
        let mut pos = 0;
        poll_fn(move |cx| {
            while pos < buf.len() {
                let mut rb = ReadBuf::new(&mut buf[pos..]);
                ready!(Pin::new(&mut *self).poll_read(cx, &mut rb))?;
                let n = rb.filled().len();
                if n == 0 {
                    return Poll::Ready(Err(io::ErrorKind::UnexpectedEof.into()));
                }
                pos += n;
            }
            Poll::Ready(Ok(pos))
        })
    }

    fn read_to_end<'a>(
        &'a mut self,
        buf: &'a mut Vec<u8>,
    ) -> impl Future<Output = Result<usize>> + 'a
    where
        Self: Unpin,
    {
        let mut st = VecInit::new(buf);
        let mut total = 0usize;
        poll_fn(move |cx| loop {
            match ready!(st.poll_read_once(Pin::new(&mut *self), cx)) {
                Err(e) => return Poll::Ready(Err(e)),
                Ok(0) => return Poll::Ready(Ok(std::mem::replace(&mut total, 0))),
                Ok(n) => total += n,
            }
        })
    }

    fn read_to_string<'a>(
        &'a mut self,
        dst: &'a mut String,
    ) -> impl Future<Output = Result<usize>> + 'a
    where
        Self: Unpin,
    {
        async move {
            // tokio と同じく、入出力のエラーなら何も足さない（read_line とは違う）
            let mut bytes = Vec::new();
            let r = self.read_to_end(&mut bytes).await?;
            finish_string_read(Ok(r), bytes, dst)
        }
    }

    fn chain<R: AsyncRead>(self, next: R) -> Chain<Self, R>
    where
        Self: Sized,
    {
        Chain {
            first: self,
            second: next,
            done_first: false,
        }
    }

    fn take(self, limit: u64) -> Take<Self>
    where
        Self: Sized,
    {
        Take { inner: self, limit }
    }

    read_num! {
        read_u8, u8, from_be_bytes;
        read_i8, i8, from_be_bytes;
        read_u16, u16, from_be_bytes;
        read_i16, i16, from_be_bytes;
        read_u32, u32, from_be_bytes;
        read_i32, i32, from_be_bytes;
        read_u64, u64, from_be_bytes;
        read_i64, i64, from_be_bytes;
        read_u128, u128, from_be_bytes;
        read_i128, i128, from_be_bytes;
        read_f32, f32, from_be_bytes;
        read_f64, f64, from_be_bytes;
        read_u16_le, u16, from_le_bytes;
        read_i16_le, i16, from_le_bytes;
        read_u32_le, u32, from_le_bytes;
        read_i32_le, i32, from_le_bytes;
        read_u64_le, u64, from_le_bytes;
        read_i64_le, i64, from_le_bytes;
        read_u128_le, u128, from_le_bytes;
        read_i128_le, i128, from_le_bytes;
        read_f32_le, f32, from_le_bytes;
        read_f64_le, f64, from_le_bytes;
    }
}

impl<R: AsyncRead + ?Sized> AsyncReadExt for R {}

/// Writes the contents of a `Buf` once (if the writer supports vectored writes, it combines up to 64 chunks).
fn poll_write_buf_once<W: AsyncWrite + ?Sized, B: bytes::Buf + ?Sized>(
    w: Pin<&mut W>,
    cx: &mut Context<'_>,
    src: &B,
) -> Poll<Result<usize>> {
    if w.is_write_vectored() {
        let mut slices = [io::IoSlice::new(&[]); 64];
        let n = src.chunks_vectored(&mut slices);
        w.poll_write_vectored(cx, &slices[..n])
    } else {
        w.poll_write(cx, src.chunk())
    }
}

/// Appends the bytes read to the string (as in tokio: even on an I/O error, the bytes read so far are appended if they are valid UTF-8).
fn finish_string_read(r: Result<usize>, bytes: Vec<u8>, dst: &mut String) -> Result<usize> {
    match (r, String::from_utf8(bytes)) {
        (Ok(n), Ok(s)) => {
            dst.push_str(&s);
            Ok(n)
        }
        (Err(e), Ok(s)) => {
            dst.push_str(&s);
            Err(e)
        }
        (Ok(_), Err(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "stream did not contain valid UTF-8",
        )),
        (Err(e), Err(_)) => Err(e),
    }
}

/// The destination of `read_to_end`. As in tokio's `VecWithInitialized`, it remembers the allocated and initialised
/// range, and once the initial capacity is filled it checks for EOF with a small read before growing.
struct VecInit<'a> {
    vec: &'a mut Vec<u8>,
    num_initialized: usize,
    starting_capacity: usize,
}

impl<'a> VecInit<'a> {
    fn new(vec: &'a mut Vec<u8>) -> VecInit<'a> {
        VecInit {
            num_initialized: vec.len(),
            starting_capacity: vec.capacity(),
            vec,
        }
    }

    fn reserve(&mut self, n: usize) {
        if self.vec.capacity() - self.vec.len() >= n {
            return;
        }
        self.num_initialized = self.vec.len();
        self.vec.reserve(n);
    }

    fn poll_read_once<R: AsyncRead + ?Sized>(
        &mut self,
        mut r: Pin<&mut R>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<usize>> {
        const NUM_BYTES: usize = 32;
        let try_small = self.vec.capacity() - self.vec.len() < NUM_BYTES
            && self.starting_capacity == self.vec.capacity()
            && self.starting_capacity >= NUM_BYTES;
        if try_small {
            let mut small = [std::mem::MaybeUninit::<u8>::uninit(); NUM_BYTES];
            let mut rb = ReadBuf::uninit(&mut small);
            let res = r.as_mut().poll_read(cx, &mut rb);
            let data = rb.filled();
            if !data.is_empty() {
                self.reserve(data.len());
                self.vec.extend_from_slice(data);
                self.num_initialized = self.num_initialized.max(self.vec.len());
            }
            let n = data.len();
            return match res {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
                Poll::Ready(Ok(())) => Poll::Ready(Ok(n)),
            };
        }
        self.reserve(NUM_BYTES);
        let len = self.vec.len();
        let cap = self.vec.capacity();
        let ptr = self.vec.as_mut_ptr().cast::<std::mem::MaybeUninit<u8>>();
        // SAFETY: 確保済みの容量全体を MaybeUninit として見る（len までは初期化済み）
        let slice = unsafe { std::slice::from_raw_parts_mut(ptr, cap) };
        let mut rb = ReadBuf::uninit(slice);
        // SAFETY: num_initialized までは初期化済み
        unsafe { rb.assume_init(self.num_initialized) };
        rb.set_filled(len);
        let res = r.as_mut().poll_read(cx, &mut rb);
        let new_len = rb.filled().len();
        let init = rb.initialized().len();
        assert_eq!(rb.filled().as_ptr(), self.vec.as_ptr());
        // SAFETY: new_len までは読み込みで初期化された
        unsafe { self.vec.set_len(new_len) };
        self.num_initialized = init;
        match res {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(())) => Poll::Ready(Ok(new_len - len)),
        }
    }
}

/// Reads into a `BufMut` (same as `poll_read_buf` in tokio-util).
pub(crate) fn poll_read_buf<R: AsyncRead + ?Sized, B: bytes::BufMut + ?Sized>(
    io: Pin<&mut R>,
    cx: &mut Context<'_>,
    buf: &mut B,
) -> Poll<Result<usize>> {
    if !buf.has_remaining_mut() {
        return Poll::Ready(Ok(0));
    }
    let n = {
        let dst = buf.chunk_mut();
        // SAFETY: UninitSlice は MaybeUninit<u8> の並び
        let dst = unsafe { &mut *(dst as *mut _ as *mut [std::mem::MaybeUninit<u8>]) };
        let mut rb = ReadBuf::uninit(dst);
        let ptr = rb.filled().as_ptr();
        ready!(io.poll_read(cx, &mut rb))?;
        assert_eq!(ptr, rb.filled().as_ptr(), "ReadBuf swapped");
        rb.filled().len()
    };
    // SAFETY: n バイトは読み込みで初期化された
    unsafe { buf.advance_mut(n) };
    Poll::Ready(Ok(n))
}

// ===================== AsyncWriteExt =====================

macro_rules! write_num {
    ($($name:ident, $t:ty, $to:ident;)*) => {
        $(
            #[doc = concat!("`", stringify!($t), "` to the stream.")]
            fn $name(&mut self, n: $t) -> impl Future<Output = Result<()>> + '_
            where
                Self: Unpin,
            {
                async move {
                    let b = n.$to();
                    self.write_all(&b).await
                }
            }
        )*
    };
}

/// `tokio::io::AsyncWriteExt`。
pub trait AsyncWriteExt: AsyncWrite {
    fn write<'a>(&'a mut self, src: &'a [u8]) -> impl Future<Output = Result<usize>> + 'a
    where
        Self: Unpin,
    {
        poll_fn(move |cx| Pin::new(&mut *self).poll_write(cx, src))
    }

    fn write_vectored<'a, 'b>(
        &'a mut self,
        bufs: &'a [io::IoSlice<'b>],
    ) -> impl Future<Output = Result<usize>> + 'a
    where
        Self: Unpin,
    {
        poll_fn(move |cx| Pin::new(&mut *self).poll_write_vectored(cx, bufs))
    }

    fn write_buf<'a, B: bytes::Buf>(
        &'a mut self,
        src: &'a mut B,
    ) -> impl Future<Output = Result<usize>> + 'a
    where
        Self: Unpin,
    {
        poll_fn(move |cx| {
            if !src.has_remaining() {
                return Poll::Ready(Ok(0));
            }
            let n = ready!(poll_write_buf_once(Pin::new(&mut *self), cx, &*src))?;
            src.advance(n);
            Poll::Ready(Ok(n))
        })
    }

    fn write_all_buf<'a, B: bytes::Buf>(
        &'a mut self,
        src: &'a mut B,
    ) -> impl Future<Output = Result<()>> + 'a
    where
        Self: Unpin,
    {
        poll_fn(move |cx| {
            while src.has_remaining() {
                let n = ready!(poll_write_buf_once(Pin::new(&mut *self), cx, &*src))?;
                if n == 0 {
                    return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
                }
                src.advance(n);
            }
            Poll::Ready(Ok(()))
        })
    }

    fn write_all<'a>(&'a mut self, src: &'a [u8]) -> impl Future<Output = Result<()>> + 'a
    where
        Self: Unpin,
    {
        let mut buf = src;
        poll_fn(move |cx| {
            while !buf.is_empty() {
                let n = ready!(Pin::new(&mut *self).poll_write(cx, buf))?;
                if n == 0 {
                    return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
                }
                buf = &buf[n..];
            }
            Poll::Ready(Ok(()))
        })
    }

    fn flush(&mut self) -> impl Future<Output = Result<()>> + '_
    where
        Self: Unpin,
    {
        poll_fn(move |cx| Pin::new(&mut *self).poll_flush(cx))
    }

    fn shutdown(&mut self) -> impl Future<Output = Result<()>> + '_
    where
        Self: Unpin,
    {
        poll_fn(move |cx| Pin::new(&mut *self).poll_shutdown(cx))
    }

    write_num! {
        write_u8, u8, to_be_bytes;
        write_i8, i8, to_be_bytes;
        write_u16, u16, to_be_bytes;
        write_i16, i16, to_be_bytes;
        write_u32, u32, to_be_bytes;
        write_i32, i32, to_be_bytes;
        write_u64, u64, to_be_bytes;
        write_i64, i64, to_be_bytes;
        write_u128, u128, to_be_bytes;
        write_i128, i128, to_be_bytes;
        write_f32, f32, to_be_bytes;
        write_f64, f64, to_be_bytes;
        write_u16_le, u16, to_le_bytes;
        write_i16_le, i16, to_le_bytes;
        write_u32_le, u32, to_le_bytes;
        write_i32_le, i32, to_le_bytes;
        write_u64_le, u64, to_le_bytes;
        write_i64_le, i64, to_le_bytes;
        write_u128_le, u128, to_le_bytes;
        write_i128_le, i128, to_le_bytes;
        write_f32_le, f32, to_le_bytes;
        write_f64_le, f64, to_le_bytes;
    }
}

impl<W: AsyncWrite + ?Sized> AsyncWriteExt for W {}

// ===================== AsyncBufReadExt =====================

/// `tokio::io::AsyncBufReadExt`。
pub trait AsyncBufReadExt: AsyncBufRead {
    fn read_until<'a>(
        &'a mut self,
        byte: u8,
        buf: &'a mut Vec<u8>,
    ) -> impl Future<Output = Result<usize>> + 'a
    where
        Self: Unpin,
    {
        let mut read = 0;
        poll_fn(move |cx| poll_read_until(Pin::new(&mut *self), cx, byte, buf, &mut read))
    }

    fn read_line<'a>(&'a mut self, buf: &'a mut String) -> impl Future<Output = Result<usize>> + 'a
    where
        Self: Unpin,
    {
        async move {
            let mut bytes = Vec::new();
            let r = self.read_until(b'\n', &mut bytes).await;
            finish_string_read(r, bytes, buf)
        }
    }

    fn split(self, byte: u8) -> Split<Self>
    where
        Self: Sized,
    {
        Split {
            reader: self,
            delim: byte,
            buf: Vec::new(),
            read: 0,
        }
    }

    fn fill_buf(&mut self) -> impl Future<Output = Result<&[u8]>> + '_
    where
        Self: Unpin,
    {
        FillBuf { reader: Some(self) }
    }

    fn consume(&mut self, amt: usize)
    where
        Self: Unpin,
    {
        Pin::new(self).consume(amt)
    }

    fn lines(self) -> Lines<Self>
    where
        Self: Sized,
    {
        Lines {
            reader: self,
            buf: Vec::new(),
            read: 0,
        }
    }
}

impl<R: AsyncBufRead + ?Sized> AsyncBufReadExt for R {}

struct FillBuf<'a, R: ?Sized> {
    reader: Option<&'a mut R>,
}

impl<'a, R: AsyncBufRead + ?Sized + Unpin> Future for FillBuf<'a, R> {
    type Output = Result<&'a [u8]>;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let reader = self.reader.take().expect("polled after completion");
        match Pin::new(&mut *reader).poll_fill_buf(cx) {
            Poll::Ready(Ok(slice)) => {
                // SAFETY: tokio と同じく、借用を延ばして返す（reader は以後使わない）
                let slice: &'a [u8] = unsafe { std::mem::transmute::<&[u8], &'a [u8]>(slice) };
                Poll::Ready(Ok(slice))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => {
                self.reader = Some(reader);
                Poll::Pending
            }
        }
    }
}

fn poll_read_until<R: AsyncBufRead + ?Sized>(
    mut reader: Pin<&mut R>,
    cx: &mut Context<'_>,
    delim: u8,
    buf: &mut Vec<u8>,
    read: &mut usize,
) -> Poll<Result<usize>> {
    loop {
        let (done, used) = {
            let available = ready!(reader.as_mut().poll_fill_buf(cx))?;
            if let Some(i) = available.iter().position(|b| *b == delim) {
                buf.extend_from_slice(&available[..=i]);
                (true, i + 1)
            } else {
                buf.extend_from_slice(available);
                (false, available.len())
            }
        };
        reader.as_mut().consume(used);
        *read += used;
        if done || used == 0 {
            return Poll::Ready(Ok(std::mem::replace(read, 0)));
        }
    }
}

/// The stream of lines returned by `lines()`.
#[derive(Debug)]
pub struct Lines<R> {
    reader: R,
    buf: Vec<u8>,
    read: usize,
}

impl<R: AsyncBufRead + Unpin> Lines<R> {
    pub async fn next_line(&mut self) -> Result<Option<String>> {
        poll_fn(|cx| self.poll_next_line(cx)).await
    }

    pub fn poll_next_line(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<String>>> {
        let n = ready!(poll_read_until(
            Pin::new(&mut self.reader),
            cx,
            b'\n',
            &mut self.buf,
            &mut self.read
        ))?;
        if n == 0 && self.buf.is_empty() {
            return Poll::Ready(Ok(None));
        }
        if self.buf.last() == Some(&b'\n') {
            self.buf.pop();
            if self.buf.last() == Some(&b'\r') {
                self.buf.pop();
            }
        }
        let bytes = std::mem::take(&mut self.buf);
        Poll::Ready(String::from_utf8(bytes).map(Some).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "stream did not contain valid UTF-8",
            )
        }))
    }

    pub fn get_ref(&self) -> &R {
        &self.reader
    }

    pub fn get_mut(&mut self) -> &mut R {
        &mut self.reader
    }

    pub fn into_inner(self) -> R {
        self.reader
    }
}

/// The stream of segments returned by `split(byte)`.
#[derive(Debug)]
pub struct Split<R> {
    reader: R,
    delim: u8,
    buf: Vec<u8>,
    read: usize,
}

impl<R: AsyncBufRead + Unpin> Split<R> {
    pub async fn next_segment(&mut self) -> Result<Option<Vec<u8>>> {
        poll_fn(|cx| self.poll_next_segment(cx)).await
    }

    pub fn poll_next_segment(&mut self, cx: &mut Context<'_>) -> Poll<Result<Option<Vec<u8>>>> {
        let n = ready!(poll_read_until(
            Pin::new(&mut self.reader),
            cx,
            self.delim,
            &mut self.buf,
            &mut self.read
        ))?;
        if n == 0 && self.buf.is_empty() {
            return Poll::Ready(Ok(None));
        }
        if self.buf.last() == Some(&self.delim) {
            self.buf.pop();
        }
        Poll::Ready(Ok(Some(std::mem::take(&mut self.buf))))
    }
}

// ===================== AsyncSeekExt =====================

/// `tokio::io::AsyncSeekExt`。
pub trait AsyncSeekExt: AsyncSeek {
    fn seek(&mut self, pos: SeekFrom) -> impl Future<Output = Result<u64>> + '_
    where
        Self: Unpin,
    {
        let mut pos = Some(pos);
        poll_fn(move |cx| {
            if let Some(p) = pos.take() {
                Pin::new(&mut *self).start_seek(p)?;
            }
            Pin::new(&mut *self).poll_complete(cx)
        })
    }

    fn rewind(&mut self) -> impl Future<Output = Result<()>> + '_
    where
        Self: Unpin,
    {
        async move { self.seek(SeekFrom::Start(0)).await.map(drop) }
    }

    fn stream_position(&mut self) -> impl Future<Output = Result<u64>> + '_
    where
        Self: Unpin,
    {
        self.seek(SeekFrom::Current(0))
    }
}

impl<S: AsyncSeek + ?Sized> AsyncSeekExt for S {}

// ===================== Chain / Take =====================

/// The chained reader returned by `chain`.
#[derive(Debug)]
pub struct Chain<T, U> {
    first: T,
    second: U,
    done_first: bool,
}

impl<T, U> Chain<T, U> {
    pub fn get_ref(&self) -> (&T, &U) {
        (&self.first, &self.second)
    }
    pub fn into_inner(self) -> (T, U) {
        (self.first, self.second)
    }
}

impl<T: AsyncRead + Unpin, U: AsyncRead + Unpin> AsyncRead for Chain<T, U> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        if !self.done_first {
            let before = buf.filled().len();
            ready!(Pin::new(&mut self.first).poll_read(cx, buf))?;
            if buf.filled().len() == before && buf.remaining() > 0 {
                self.done_first = true;
            } else {
                return Poll::Ready(Ok(()));
            }
        }
        Pin::new(&mut self.second).poll_read(cx, buf)
    }
}

/// The limited reader returned by `take`.
#[derive(Debug)]
pub struct Take<R> {
    inner: R,
    limit: u64,
}

impl<R> Take<R> {
    pub fn limit(&self) -> u64 {
        self.limit
    }
    pub fn set_limit(&mut self, limit: u64) {
        self.limit = limit;
    }
    pub fn get_ref(&self) -> &R {
        &self.inner
    }
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.inner
    }
    pub fn into_inner(self) -> R {
        self.inner
    }
}

// B-170: tokio と同じく Unpin でない読み手も受ける（tower-http の ServeDir が `Take<File の包み>` を使う）
impl<R: AsyncRead> AsyncRead for Take<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        // SAFETY: Take は inner を構造的にピン留めする（pin-project と同じ形）。inner を Pin の外へ動かす
        // メソッドは `&mut self`・`self` を取るもの（ピン留めされていない Take だけが呼べる）に限り、
        // Take は Drop も Unpin も手で実装しない（Unpin は R が Unpin のときだけ自動で付く）。
        let this = unsafe { self.get_unchecked_mut() };
        if this.limit == 0 {
            return Poll::Ready(Ok(()));
        }
        let max = std::cmp::min(buf.remaining() as u64, this.limit) as usize;
        let mut b = buf.take(max);
        let ptr = b.filled().as_ptr();
        // SAFETY: 上と同じ（inner は this と一緒にピン留めされたまま動かない）
        let inner = unsafe { Pin::new_unchecked(&mut this.inner) };
        ready!(inner.poll_read(cx, &mut b))?;
        assert_eq!(ptr, b.filled().as_ptr());
        let n = b.filled().len();
        // SAFETY: n バイトは読み込みで初期化された
        unsafe { buf.assume_init(n) };
        buf.advance(n);
        this.limit -= n as u64;
        Poll::Ready(Ok(()))
    }
}

// ===================== empty / sink / repeat =====================

/// A reader that is always at EOF, and a writer that discards everything.
#[derive(Debug, Default, Clone, Copy)]
pub struct Empty(());

pub fn empty() -> Empty {
    Empty(())
}

impl AsyncRead for Empty {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        _: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        ready!(crate::tokio::task::coop::poll_proceed_and_make_progress(cx));
        Poll::Ready(Ok(()))
    }
}

impl AsyncBufRead for Empty {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<&[u8]>> {
        ready!(crate::tokio::task::coop::poll_proceed_and_make_progress(cx));
        Poll::Ready(Ok(&[]))
    }
    fn consume(self: Pin<&mut Self>, _: usize) {}
}

impl AsyncWrite for Empty {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize>> {
        ready!(crate::tokio::task::coop::poll_proceed_and_make_progress(cx));
        Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        ready!(crate::tokio::task::coop::poll_proceed_and_make_progress(cx));
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        ready!(crate::tokio::task::coop::poll_proceed_and_make_progress(cx));
        Poll::Ready(Ok(()))
    }
}

impl AsyncSeek for Empty {
    fn start_seek(self: Pin<&mut Self>, _: SeekFrom) -> Result<()> {
        Ok(())
    }
    fn poll_complete(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<u64>> {
        Poll::Ready(Ok(0))
    }
}

/// A writer that discards everything.
#[derive(Debug, Default, Clone, Copy)]
pub struct Sink(());

pub fn sink() -> Sink {
    Sink(())
}

impl AsyncWrite for Sink {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize>> {
        ready!(crate::tokio::task::coop::poll_proceed_and_make_progress(cx));
        Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        ready!(crate::tokio::task::coop::poll_proceed_and_make_progress(cx));
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        ready!(crate::tokio::task::coop::poll_proceed_and_make_progress(cx));
        Poll::Ready(Ok(()))
    }
}

/// A reader that returns the same byte forever.
#[derive(Debug, Clone, Copy)]
pub struct Repeat(u8);

pub fn repeat(byte: u8) -> Repeat {
    Repeat(byte)
}

impl AsyncRead for Repeat {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        ready!(crate::tokio::task::coop::poll_proceed_and_make_progress(cx));
        let n = buf.remaining();
        let b = self.0;
        for x in buf.initialize_unfilled_to(n) {
            *x = b;
        }
        buf.advance(n);
        Poll::Ready(Ok(()))
    }
}

// ===================== BufReader / BufWriter / BufStream =====================

const DEFAULT_BUF_SIZE: usize = 8 * 1024;

/// `tokio::io::BufReader`。
#[derive(Debug)]
pub struct BufReader<R> {
    inner: R,
    buf: Box<[u8]>,
    pos: usize,
    cap: usize,
    seek_state: ReaderSeek,
}

#[derive(Debug, Clone, Copy)]
enum ReaderSeek {
    Init,
    Start(SeekFrom),
    PendingOverflowed(i64),
    Pending,
}

impl<R: AsyncRead> BufReader<R> {
    pub fn new(inner: R) -> BufReader<R> {
        BufReader::with_capacity(DEFAULT_BUF_SIZE, inner)
    }

    pub fn with_capacity(capacity: usize, inner: R) -> BufReader<R> {
        BufReader {
            inner,
            buf: vec![0; capacity].into_boxed_slice(),
            pos: 0,
            cap: 0,
            seek_state: ReaderSeek::Init,
        }
    }
}

impl<R> BufReader<R> {
    pub fn get_ref(&self) -> &R {
        &self.inner
    }
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.inner
    }
    pub fn get_pin_mut(self: Pin<&mut Self>) -> Pin<&mut R> {
        // SAFETY: inner は構造的にピン留めされる
        unsafe { self.map_unchecked_mut(|s| &mut s.inner) }
    }
    pub fn into_inner(self) -> R {
        self.inner
    }
    pub fn buffer(&self) -> &[u8] {
        &self.buf[self.pos..self.cap]
    }

    fn discard_buffer(self: Pin<&mut Self>) {
        // SAFETY: inner を動かさない
        let me = unsafe { self.get_unchecked_mut() };
        me.pos = 0;
        me.cap = 0;
    }
}

impl<R: AsyncRead> AsyncRead for BufReader<R> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        if self.pos == self.cap && buf.remaining() >= self.buf.len() {
            let r = ready!(self.as_mut().get_pin_mut().poll_read(cx, buf));
            self.discard_buffer();
            return Poll::Ready(r);
        }
        let rem = ready!(self.as_mut().poll_fill_buf(cx))?;
        let amt = std::cmp::min(rem.len(), buf.remaining());
        buf.put_slice(&rem[..amt]);
        self.consume(amt);
        Poll::Ready(Ok(()))
    }
}

impl<R: AsyncRead> AsyncBufRead for BufReader<R> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<&[u8]>> {
        // SAFETY: inner は構造的ピン留め、他のフィールドは Unpin
        let me = unsafe { self.get_unchecked_mut() };
        if me.pos >= me.cap {
            let mut rb = ReadBuf::new(&mut me.buf);
            ready!(unsafe { Pin::new_unchecked(&mut me.inner) }.poll_read(cx, &mut rb))?;
            me.cap = rb.filled().len();
            me.pos = 0;
        }
        Poll::Ready(Ok(&me.buf[me.pos..me.cap]))
    }

    fn consume(self: Pin<&mut Self>, amt: usize) {
        // SAFETY: inner を動かさない
        let me = unsafe { self.get_unchecked_mut() };
        me.pos = std::cmp::min(me.pos + amt, me.cap);
    }
}

impl<R: AsyncRead + AsyncWrite> AsyncWrite for BufReader<R> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize>> {
        self.get_pin_mut().poll_write(cx, buf)
    }
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<Result<usize>> {
        self.get_pin_mut().poll_write_vectored(cx, bufs)
    }
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        self.get_pin_mut().poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        self.get_pin_mut().poll_shutdown(cx)
    }
}

impl<R: AsyncRead + AsyncSeek> AsyncSeek for BufReader<R> {
    fn start_seek(self: Pin<&mut Self>, pos: SeekFrom) -> Result<()> {
        // SAFETY: inner を動かさない
        unsafe { self.get_unchecked_mut() }.seek_state = ReaderSeek::Start(pos);
        Ok(())
    }

    fn poll_complete(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<u64>> {
        // SAFETY: inner を動かさない（seek_state は Unpin）
        let state = std::mem::replace(
            &mut unsafe { self.as_mut().get_unchecked_mut() }.seek_state,
            ReaderSeek::Init,
        );
        let set = |me: &mut Pin<&mut Self>, st| unsafe { me.as_mut().get_unchecked_mut() }.seek_state = st;
        let res = match state {
            ReaderSeek::Init => return Poll::Ready(Ok(0)),
            ReaderSeek::Start(SeekFrom::Current(n)) => {
                let remainder = (self.cap - self.pos) as i64;
                if let Some(offset) = n.checked_sub(remainder) {
                    self.as_mut()
                        .get_pin_mut()
                        .start_seek(SeekFrom::Current(offset))?;
                } else {
                    self.as_mut()
                        .get_pin_mut()
                        .start_seek(SeekFrom::Current(-remainder))?;
                    if self.as_mut().get_pin_mut().poll_complete(cx)?.is_pending() {
                        set(&mut self, ReaderSeek::PendingOverflowed(n));
                        return Poll::Pending;
                    }
                    self.as_mut().discard_buffer();
                    self.as_mut()
                        .get_pin_mut()
                        .start_seek(SeekFrom::Current(n))?;
                }
                self.as_mut().get_pin_mut().poll_complete(cx)?
            }
            ReaderSeek::PendingOverflowed(n) => {
                if self.as_mut().get_pin_mut().poll_complete(cx)?.is_pending() {
                    set(&mut self, ReaderSeek::PendingOverflowed(n));
                    return Poll::Pending;
                }
                self.as_mut().discard_buffer();
                self.as_mut()
                    .get_pin_mut()
                    .start_seek(SeekFrom::Current(n))?;
                self.as_mut().get_pin_mut().poll_complete(cx)?
            }
            ReaderSeek::Start(pos) => {
                self.as_mut().get_pin_mut().start_seek(pos)?;
                self.as_mut().get_pin_mut().poll_complete(cx)?
            }
            ReaderSeek::Pending => self.as_mut().get_pin_mut().poll_complete(cx)?,
        };
        match res {
            Poll::Ready(r) => {
                self.discard_buffer();
                Poll::Ready(Ok(r))
            }
            Poll::Pending => {
                set(&mut self, ReaderSeek::Pending);
                Poll::Pending
            }
        }
    }
}

/// `tokio::io::BufWriter`。
#[derive(Debug)]
pub struct BufWriter<W> {
    inner: W,
    buf: Vec<u8>,
    written: usize,
    cap: usize,
    seek_state: WriterSeek,
}

#[derive(Debug, Clone, Copy)]
enum WriterSeek {
    Init,
    Start(SeekFrom),
    Pending,
}

impl<W: AsyncWrite> BufWriter<W> {
    pub fn new(inner: W) -> BufWriter<W> {
        BufWriter::with_capacity(DEFAULT_BUF_SIZE, inner)
    }

    pub fn with_capacity(cap: usize, inner: W) -> BufWriter<W> {
        BufWriter {
            inner,
            buf: Vec::with_capacity(cap),
            written: 0,
            cap,
            seek_state: WriterSeek::Init,
        }
    }

    fn flush_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        // SAFETY: inner は構造的ピン留め
        let me = unsafe { self.get_unchecked_mut() };
        let mut ret = Ok(());
        while me.written < me.buf.len() {
            match ready!(
                unsafe { Pin::new_unchecked(&mut me.inner) }.poll_write(cx, &me.buf[me.written..])
            ) {
                Ok(0) => {
                    ret = Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to write the buffered data",
                    ));
                    break;
                }
                Ok(n) => me.written += n,
                Err(e) => {
                    ret = Err(e);
                    break;
                }
            }
        }
        if me.written > 0 {
            me.buf.drain(..me.written);
        }
        me.written = 0;
        Poll::Ready(ret)
    }
}

impl<W> BufWriter<W> {
    pub fn get_ref(&self) -> &W {
        &self.inner
    }
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.inner
    }
    pub fn get_pin_mut(self: Pin<&mut Self>) -> Pin<&mut W> {
        // SAFETY: inner は構造的ピン留め
        unsafe { self.map_unchecked_mut(|s| &mut s.inner) }
    }
    pub fn into_inner(self) -> W {
        self.inner
    }
    pub fn buffer(&self) -> &[u8] {
        &self.buf
    }
}

impl<W: AsyncWrite> AsyncWrite for BufWriter<W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize>> {
        if self.buf.len() + buf.len() > self.cap {
            ready!(self.as_mut().flush_buf(cx))?;
        }
        // SAFETY: inner を動かさない
        let me = unsafe { self.get_unchecked_mut() };
        if buf.len() >= me.cap {
            unsafe { Pin::new_unchecked(&mut me.inner) }.poll_write(cx, buf)
        } else {
            me.buf.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut bufs: &[io::IoSlice<'_>],
    ) -> Poll<Result<usize>> {
        let cap = self.cap;
        if self.inner.is_write_vectored() {
            let total = bufs.iter().fold(0usize, |a, b| a.saturating_add(b.len()));
            if total > cap - self.buf.len() {
                ready!(self.as_mut().flush_buf(cx))?;
            }
            // SAFETY: inner を動かさない
            let me = unsafe { self.get_unchecked_mut() };
            if total >= cap {
                unsafe { Pin::new_unchecked(&mut me.inner) }.poll_write_vectored(cx, bufs)
            } else {
                bufs.iter().for_each(|b| me.buf.extend_from_slice(b));
                Poll::Ready(Ok(total))
            }
        } else {
            while bufs.first().map(|b| b.len()) == Some(0) {
                bufs = &bufs[1..];
            }
            if bufs.is_empty() {
                return Poll::Ready(Ok(0));
            }
            let first = bufs[0].len();
            if first > cap - self.buf.len() {
                ready!(self.as_mut().flush_buf(cx))?;
            }
            // SAFETY: inner を動かさない
            let me = unsafe { self.get_unchecked_mut() };
            if first >= cap {
                return unsafe { Pin::new_unchecked(&mut me.inner) }.poll_write(cx, &bufs[0]);
            }
            me.buf.extend_from_slice(&bufs[0]);
            let mut total = first;
            for b in &bufs[1..] {
                if b.len() > cap - me.buf.len() {
                    break;
                }
                me.buf.extend_from_slice(b);
                total += b.len();
            }
            Poll::Ready(Ok(total))
        }
    }

    fn is_write_vectored(&self) -> bool {
        true
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        ready!(self.as_mut().flush_buf(cx))?;
        self.get_pin_mut().poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        ready!(self.as_mut().flush_buf(cx))?;
        self.get_pin_mut().poll_shutdown(cx)
    }
}

impl<W: AsyncWrite + AsyncSeek> AsyncSeek for BufWriter<W> {
    fn start_seek(self: Pin<&mut Self>, pos: SeekFrom) -> Result<()> {
        // SAFETY: inner を動かさない
        unsafe { self.get_unchecked_mut() }.seek_state = WriterSeek::Start(pos);
        Ok(())
    }

    fn poll_complete(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<u64>> {
        let pos = match self.seek_state {
            WriterSeek::Init => return self.get_pin_mut().poll_complete(cx),
            WriterSeek::Start(p) => Some(p),
            WriterSeek::Pending => None,
        };
        ready!(self.as_mut().flush_buf(cx))?;
        if let Some(p) = pos {
            ready!(self.as_mut().get_pin_mut().poll_complete(cx))?;
            if let Err(e) = self.as_mut().get_pin_mut().start_seek(p) {
                // SAFETY: inner を動かさない
                unsafe { self.as_mut().get_unchecked_mut() }.seek_state = WriterSeek::Init;
                return Poll::Ready(Err(e));
            }
        }
        let r = self.as_mut().get_pin_mut().poll_complete(cx);
        // SAFETY: 同上
        unsafe { self.as_mut().get_unchecked_mut() }.seek_state = if r.is_ready() {
            WriterSeek::Init
        } else {
            WriterSeek::Pending
        };
        r
    }
}

impl<W: AsyncWrite + AsyncRead> AsyncRead for BufWriter<W> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        self.get_pin_mut().poll_read(cx, buf)
    }
}

impl<W: AsyncWrite + AsyncBufRead> AsyncBufRead for BufWriter<W> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<&[u8]>> {
        self.get_pin_mut().poll_fill_buf(cx)
    }
    fn consume(self: Pin<&mut Self>, amt: usize) {
        self.get_pin_mut().consume(amt)
    }
}

/// Buffers both reading and writing (`BufReader<BufWriter<RW>>`).
#[derive(Debug)]
pub struct BufStream<RW> {
    inner: BufReader<BufWriter<RW>>,
}

impl<RW: AsyncRead + AsyncWrite> BufStream<RW> {
    pub fn new(stream: RW) -> BufStream<RW> {
        BufStream {
            inner: BufReader::new(BufWriter::new(stream)),
        }
    }

    pub fn get_ref(&self) -> &RW {
        self.inner.get_ref().get_ref()
    }

    pub fn get_mut(&mut self) -> &mut RW {
        self.inner.get_mut().get_mut()
    }

    pub fn into_inner(self) -> RW {
        self.inner.into_inner().into_inner()
    }
}

impl<RW: AsyncRead + AsyncWrite> AsyncRead for BufStream<RW> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        // SAFETY: inner は構造的ピン留め
        unsafe { self.map_unchecked_mut(|s| &mut s.inner) }.poll_read(cx, buf)
    }
}

impl<RW: AsyncRead + AsyncWrite> AsyncWrite for BufStream<RW> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize>> {
        // SAFETY: inner は構造的ピン留め
        unsafe { self.map_unchecked_mut(|s| &mut s.inner) }.poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        // SAFETY: 同上
        unsafe { self.map_unchecked_mut(|s| &mut s.inner) }.poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        // SAFETY: 同上
        unsafe { self.map_unchecked_mut(|s| &mut s.inner) }.poll_shutdown(cx)
    }
}

impl<RW: AsyncRead + AsyncWrite> AsyncBufRead for BufStream<RW> {
    fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<&[u8]>> {
        // SAFETY: 同上
        unsafe { self.map_unchecked_mut(|s| &mut s.inner) }.poll_fill_buf(cx)
    }
    fn consume(self: Pin<&mut Self>, amt: usize) {
        // SAFETY: 同上
        unsafe { self.map_unchecked_mut(|s| &mut s.inner) }.consume(amt)
    }
}

// ===================== copy =====================

struct CopyBuffer {
    buf: Box<[u8]>,
    pos: usize,
    cap: usize,
    amt: u64,
    read_done: bool,
    need_flush: bool,
}

impl CopyBuffer {
    fn new(size: usize) -> CopyBuffer {
        CopyBuffer {
            buf: vec![0; size].into_boxed_slice(),
            pos: 0,
            cap: 0,
            amt: 0,
            read_done: false,
            need_flush: false,
        }
    }

    fn poll_copy<R: AsyncRead + ?Sized, W: AsyncWrite + ?Sized>(
        &mut self,
        cx: &mut Context<'_>,
        mut reader: Pin<&mut R>,
        mut writer: Pin<&mut W>,
    ) -> Poll<Result<u64>> {
        loop {
            // tokio と同じく、1 周ごとに予算を使う（使い切ったら書き出しを flush して譲る）
            if crate::tokio::task::coop::poll_proceed_and_make_progress(cx).is_pending() {
                if self.need_flush {
                    ready!(writer.as_mut().poll_flush(cx))?;
                    self.need_flush = false;
                }
                return Poll::Pending;
            }
            if self.pos == self.cap && !self.read_done {
                self.pos = 0;
                self.cap = 0;
                let mut rb = ReadBuf::new(&mut self.buf);
                match reader.as_mut().poll_read(cx, &mut rb) {
                    Poll::Ready(Ok(())) => {}
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => {
                        if self.need_flush {
                            ready!(writer.as_mut().poll_flush(cx))?;
                            self.need_flush = false;
                        }
                        return Poll::Pending;
                    }
                }
                let n = rb.filled().len();
                if n == 0 {
                    self.read_done = true;
                } else {
                    self.cap = n;
                }
            }
            while self.pos < self.cap {
                let n = ready!(writer
                    .as_mut()
                    .poll_write(cx, &self.buf[self.pos..self.cap]))?;
                if n == 0 {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "write zero byte into writer",
                    )));
                }
                self.pos += n;
                self.amt += n as u64;
                self.need_flush = true;
            }
            if self.pos == self.cap && self.read_done {
                ready!(writer.as_mut().poll_flush(cx))?;
                return Poll::Ready(Ok(self.amt));
            }
        }
    }
}

/// Copies from a reader to a writer until EOF. Returns the number of bytes copied.
pub async fn copy<'a, R, W>(reader: &'a mut R, writer: &'a mut W) -> Result<u64>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    let mut b = CopyBuffer::new(DEFAULT_BUF_SIZE);
    poll_fn(|cx| b.poll_copy(cx, Pin::new(&mut *reader), Pin::new(&mut *writer))).await
}

/// Copies from an `AsyncBufRead`.
pub async fn copy_buf<'a, R, W>(reader: &'a mut R, writer: &'a mut W) -> Result<u64>
where
    R: AsyncBufRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    let mut amt = 0u64;
    poll_fn(|cx| loop {
        let buf = ready!(Pin::new(&mut *reader).poll_fill_buf(cx))?;
        if buf.is_empty() {
            ready!(Pin::new(&mut *writer).poll_flush(cx))?;
            return Poll::Ready(Ok(amt));
        }
        let n = ready!(Pin::new(&mut *writer).poll_write(cx, buf))?;
        if n == 0 {
            return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
        }
        amt += n as u64;
        Pin::new(&mut *reader).consume(n);
    })
    .await
}

/// Copies in both directions until both finish, shutting down the other side when one side reaches EOF. Returns the number of bytes copied as (a to b, b to a).
pub async fn copy_bidirectional<A, B>(a: &mut A, b: &mut B) -> Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    copy_bidirectional_with_sizes(a, b, DEFAULT_BUF_SIZE, DEFAULT_BUF_SIZE).await
}

pub async fn copy_bidirectional_with_sizes<A, B>(
    a: &mut A,
    b: &mut B,
    a_to_b: usize,
    b_to_a: usize,
) -> Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    enum St {
        Copying(CopyBuffer),
        ShuttingDown(u64),
        Done(u64),
    }
    fn step<R, W>(st: &mut St, cx: &mut Context<'_>, r: &mut R, w: &mut W) -> Poll<Result<u64>>
    where
        R: AsyncRead + Unpin + ?Sized,
        W: AsyncWrite + Unpin + ?Sized,
    {
        loop {
            match st {
                St::Copying(buf) => {
                    let n = ready!(buf.poll_copy(cx, Pin::new(&mut *r), Pin::new(&mut *w)))?;
                    *st = St::ShuttingDown(n);
                }
                St::ShuttingDown(n) => {
                    ready!(Pin::new(&mut *w).poll_shutdown(cx))?;
                    *st = St::Done(*n);
                }
                St::Done(n) => return Poll::Ready(Ok(*n)),
            }
        }
    }
    let mut ab = St::Copying(CopyBuffer::new(a_to_b));
    let mut ba = St::Copying(CopyBuffer::new(b_to_a));
    poll_fn(|cx| {
        let r1 = step(&mut ab, cx, &mut *a, &mut *b)?;
        let r2 = step(&mut ba, cx, &mut *b, &mut *a)?;
        match (r1, r2) {
            (Poll::Ready(x), Poll::Ready(y)) => Poll::Ready(Ok((x, y))),
            _ => Poll::Pending,
        }
    })
    .await
}

// ===================== split =====================

struct SplitInner<T> {
    stream: Mutex<T>,
}

/// The read half.
pub struct ReadHalf<T> {
    inner: Arc<SplitInner<T>>,
}

/// The write half.
pub struct WriteHalf<T> {
    inner: Arc<SplitInner<T>>,
}

impl<T> std::fmt::Debug for ReadHalf<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReadHalf")
    }
}

impl<T> std::fmt::Debug for WriteHalf<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WriteHalf")
    }
}

/// Splits into a read half and a write half (join them again with `unsplit`).
pub fn split<T: AsyncRead + AsyncWrite>(stream: T) -> (ReadHalf<T>, WriteHalf<T>) {
    let inner = Arc::new(SplitInner {
        stream: Mutex::new(stream),
    });
    (
        ReadHalf {
            inner: inner.clone(),
        },
        WriteHalf { inner },
    )
}

impl<T> ReadHalf<T> {
    pub fn is_pair_of(&self, other: &WriteHalf<T>) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    #[track_caller]
    pub fn unsplit(self, wr: WriteHalf<T>) -> T
    where
        T: Unpin,
    {
        assert!(
            self.is_pair_of(&wr),
            "Unrelated `split::Write` passed to `split::Read::unsplit`."
        );
        drop(wr);
        let inner = Arc::try_unwrap(self.inner)
            .ok()
            .expect("`Arc::try_unwrap` failed");
        inner.stream.into_inner().unwrap()
    }
}

impl<T> WriteHalf<T> {
    pub fn is_pair_of(&self, other: &ReadHalf<T>) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl<T: AsyncRead> AsyncRead for ReadHalf<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        let mut g = self.inner.stream.lock().unwrap();
        // SAFETY: Arc の中で動かさない
        unsafe { Pin::new_unchecked(&mut *g) }.poll_read(cx, buf)
    }
}

impl<T: AsyncWrite> AsyncWrite for WriteHalf<T> {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize>> {
        let mut g = self.inner.stream.lock().unwrap();
        // SAFETY: 同上
        unsafe { Pin::new_unchecked(&mut *g) }.poll_write(cx, buf)
    }
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<Result<usize>> {
        let mut g = self.inner.stream.lock().unwrap();
        // SAFETY: 同上
        unsafe { Pin::new_unchecked(&mut *g) }.poll_write_vectored(cx, bufs)
    }
    fn is_write_vectored(&self) -> bool {
        self.inner.stream.lock().unwrap().is_write_vectored()
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        let mut g = self.inner.stream.lock().unwrap();
        // SAFETY: 同上
        unsafe { Pin::new_unchecked(&mut *g) }.poll_flush(cx)
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        let mut g = self.inner.stream.lock().unwrap();
        // SAFETY: 同上
        unsafe { Pin::new_unchecked(&mut *g) }.poll_shutdown(cx)
    }
}

/// Combines a reader and a writer into one value.
#[derive(Debug)]
pub struct Join<R, W> {
    reader: R,
    writer: W,
}

pub fn join<R: AsyncRead, W: AsyncWrite>(reader: R, writer: W) -> Join<R, W> {
    Join { reader, writer }
}

impl<R, W> Join<R, W> {
    pub fn into_inner(self) -> (R, W) {
        (self.reader, self.writer)
    }
    pub fn reader(&self) -> &R {
        &self.reader
    }
    pub fn writer(&self) -> &W {
        &self.writer
    }
}

impl<R: AsyncRead + Unpin, W: Unpin> AsyncRead for Join<R, W> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        Pin::new(&mut self.reader).poll_read(cx, buf)
    }
}

impl<R: Unpin, W: AsyncWrite + Unpin> AsyncWrite for Join<R, W> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize>> {
        Pin::new(&mut self.writer).poll_write(cx, buf)
    }
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<Result<usize>> {
        Pin::new(&mut self.writer).poll_write_vectored(cx, bufs)
    }
    fn is_write_vectored(&self) -> bool {
        self.writer.is_write_vectored()
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        Pin::new(&mut self.writer).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<()>> {
        Pin::new(&mut self.writer).poll_shutdown(cx)
    }
}

// ===================== duplex / simplex =====================

struct Pipe {
    buf: std::collections::VecDeque<u8>,
    max: usize,
    is_closed: bool,
    read_waker: WakerSlot,
    write_waker: WakerSlot,
}

impl Pipe {
    fn new(max: usize) -> Pipe {
        Pipe {
            buf: std::collections::VecDeque::new(),
            max,
            is_closed: false,
            read_waker: WakerSlot::default(),
            write_waker: WakerSlot::default(),
        }
    }

    fn close_write(&mut self) {
        self.is_closed = true;
        self.read_waker.wake();
    }

    fn close_read(&mut self) {
        self.is_closed = true;
        self.write_waker.wake();
    }

    fn poll_read(&mut self, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<Result<()>> {
        let coop = ready!(crate::tokio::task::coop::poll_proceed(cx));
        if !self.buf.is_empty() {
            coop.made_progress();
            let n = std::cmp::min(self.buf.len(), buf.remaining());
            let (a, b) = self.buf.as_slices();
            let na = std::cmp::min(a.len(), n);
            buf.put_slice(&a[..na]);
            if n > na {
                buf.put_slice(&b[..n - na]);
            }
            self.buf.drain(..n);
            self.write_waker.wake();
            return Poll::Ready(Ok(()));
        }
        if self.is_closed {
            coop.made_progress();
            return Poll::Ready(Ok(()));
        }
        self.read_waker.register(cx.waker());
        Poll::Pending
    }

    fn poll_write_vectored(
        &mut self,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<Result<usize>> {
        ready!(crate::tokio::task::coop::poll_proceed_and_make_progress(cx));
        if self.is_closed {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        let mut avail = self.max - self.buf.len();
        if avail == 0 {
            self.write_waker.register(cx.waker());
            return Poll::Pending;
        }
        let mut n = 0;
        for b in bufs {
            let k = std::cmp::min(avail, b.len());
            self.buf.extend(&b[..k]);
            n += k;
            avail -= k;
            if avail == 0 {
                break;
            }
        }
        self.read_waker.wake();
        Poll::Ready(Ok(n))
    }

    fn poll_write(&mut self, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize>> {
        ready!(crate::tokio::task::coop::poll_proceed_and_make_progress(cx));
        if self.is_closed {
            return Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()));
        }
        let avail = self.max - self.buf.len();
        if avail == 0 {
            self.write_waker.register(cx.waker());
            return Poll::Pending;
        }
        let n = std::cmp::min(avail, buf.len());
        self.buf.extend(&buf[..n]);
        self.read_waker.wake();
        Poll::Ready(Ok(n))
    }
}

/// One end of an in-memory bidirectional stream (see `duplex`).
#[derive(Debug)]
pub struct DuplexStream {
    read: Arc<Mutex<Pipe>>,
    write: Arc<Mutex<Pipe>>,
}

impl std::fmt::Debug for Pipe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pipe")
            .field("len", &self.buf.len())
            .finish()
    }
}

/// Creates both ends of a bidirectional stream with capacity `max_buf_size`.
pub fn duplex(max_buf_size: usize) -> (DuplexStream, DuplexStream) {
    let one = Arc::new(Mutex::new(Pipe::new(max_buf_size)));
    let two = Arc::new(Mutex::new(Pipe::new(max_buf_size)));
    (
        DuplexStream {
            read: one.clone(),
            write: two.clone(),
        },
        DuplexStream {
            read: two,
            write: one,
        },
    )
}

impl AsyncRead for DuplexStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        self.read.lock().unwrap().poll_read(cx, buf)
    }
}

impl AsyncWrite for DuplexStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<Result<usize>> {
        self.write.lock().unwrap().poll_write(cx, buf)
    }
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<Result<usize>> {
        self.write.lock().unwrap().poll_write_vectored(cx, bufs)
    }
    fn is_write_vectored(&self) -> bool {
        true
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<()>> {
        self.write.lock().unwrap().close_write();
        Poll::Ready(Ok(()))
    }
}

impl Drop for DuplexStream {
    fn drop(&mut self) {
        self.write.lock().unwrap().close_write();
        self.read.lock().unwrap().close_read();
    }
}

/// A one-directional stream (see `simplex`).
#[derive(Debug)]
pub struct SimplexStream {
    pipe: Pipe,
}

pub fn simplex(max_buf_size: usize) -> (ReadHalf<SimplexStream>, WriteHalf<SimplexStream>) {
    split(SimplexStream {
        pipe: Pipe::new(max_buf_size),
    })
}

impl SimplexStream {
    pub fn new_unsplit(max_buf_size: usize) -> SimplexStream {
        SimplexStream {
            pipe: Pipe::new(max_buf_size),
        }
    }
}

impl AsyncRead for SimplexStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        self.pipe.poll_read(cx, buf)
    }
}

impl AsyncWrite for SimplexStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize>> {
        self.pipe.poll_write(cx, buf)
    }
    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<()>> {
        self.pipe.close_write();
        Poll::Ready(Ok(()))
    }
}

// ===================== 標準入出力 =====================

/// Standard output. Writes are small synchronous writes, so it is not suited to large amounts of output.
#[derive(Debug)]
pub struct Stdout(());

/// Standard error.
#[derive(Debug)]
pub struct Stderr(());

pub fn stdout() -> Stdout {
    Stdout(())
}

pub fn stderr() -> Stderr {
    Stderr(())
}

macro_rules! std_writer {
    ($t:ty, $f:expr) => {
        impl AsyncWrite for $t {
            fn poll_write(
                self: Pin<&mut Self>,
                _: &mut Context<'_>,
                buf: &[u8],
            ) -> Poll<Result<usize>> {
                Poll::Ready(std::io::Write::write(&mut $f, buf))
            }
            fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<()>> {
                Poll::Ready(std::io::Write::flush(&mut $f))
            }
            fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Result<()>> {
                Poll::Ready(Ok(()))
            }
        }
    };
}

std_writer!(Stdout, std::io::stdout());
std_writer!(Stderr, std::io::stderr());

/// Standard input (read on a dedicated thread).
#[derive(Debug)]
pub struct Stdin {
    rx: crate::tokio::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    pending: Vec<u8>,
    eof: bool,
}

pub fn stdin() -> Stdin {
    let (tx, rx) = crate::tokio::sync::mpsc::channel(4);
    let _ = std::thread::Builder::new()
        .name("veil-tokio-stdin".into())
        .spawn(move || loop {
            let mut buf = vec![0u8; 8192];
            let r = std::io::Read::read(&mut std::io::stdin(), &mut buf);
            let done = !matches!(r, Ok(n) if n > 0);
            let item = r.map(|n| {
                buf.truncate(n);
                buf
            });
            if tx.blocking_send(item).is_err() || done {
                break;
            }
        });
    Stdin {
        rx,
        pending: Vec::new(),
        eof: false,
    }
}

impl AsyncRead for Stdin {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<()>> {
        if self.pending.is_empty() && !self.eof {
            match ready!(self.rx.poll_recv(cx)) {
                Some(Ok(v)) if !v.is_empty() => self.pending = v,
                Some(Err(e)) => return Poll::Ready(Err(e)),
                _ => self.eof = true,
            }
        }
        let n = std::cmp::min(self.pending.len(), buf.remaining());
        buf.put_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Poll::Ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unpin でない読み手（tower-http の ServeDir が包むファイルと同じ形）。
    struct NotUnpin {
        data: &'static [u8],
        _pin: std::marker::PhantomPinned,
    }

    impl AsyncRead for NotUnpin {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<Result<()>> {
            // SAFETY: data（参照）を読むだけで、自分を動かさない
            let this = unsafe { self.get_unchecked_mut() };
            let n = buf.remaining().min(this.data.len());
            buf.put_slice(&this.data[..n]);
            this.data = &this.data[n..];
            Poll::Ready(Ok(()))
        }
    }

    // B-170: Take は Unpin でない読み手でも読める（上限で止まる）
    #[test]
    fn take_reads_not_unpin_reader_up_to_limit() {
        let r = NotUnpin {
            data: b"hello world",
            _pin: std::marker::PhantomPinned,
        };
        let mut t = Box::pin(Take { inner: r, limit: 5 });
        let waker = futures::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        let mut out = [0u8; 16];
        let mut got = Vec::new();
        loop {
            let mut b = ReadBuf::new(&mut out);
            match t.as_mut().poll_read(&mut cx, &mut b) {
                Poll::Ready(Ok(())) if b.filled().is_empty() => break,
                Poll::Ready(Ok(())) => got.extend_from_slice(b.filled()),
                other => panic!("unexpected {:?}", other.map(|r| r.is_ok())),
            }
        }
        assert_eq!(got, b"hello");
    }
}
