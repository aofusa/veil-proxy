//! Equivalent of `tokio::io::ReadBuf`: a buffer that tracks its initialised and filled ranges.

use std::mem::MaybeUninit;

/// The buffer to read into (the methods have the same meaning as in tokio).
pub struct ReadBuf<'a> {
    buf: &'a mut [MaybeUninit<u8>],
    filled: usize,
    initialized: usize,
}

impl std::fmt::Debug for ReadBuf<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadBuf")
            .field("filled", &self.filled)
            .field("initialized", &self.initialized)
            .field("capacity", &self.capacity())
            .finish()
    }
}

impl<'a> ReadBuf<'a> {
    pub fn new(buf: &'a mut [u8]) -> ReadBuf<'a> {
        let initialized = buf.len();
        // SAFETY: 初期化済みの u8 を MaybeUninit<u8> として見るのは常に安全
        let buf = unsafe { &mut *(buf as *mut [u8] as *mut [MaybeUninit<u8>]) };
        ReadBuf {
            buf,
            filled: 0,
            initialized,
        }
    }

    pub fn uninit(buf: &'a mut [MaybeUninit<u8>]) -> ReadBuf<'a> {
        ReadBuf {
            buf,
            filled: 0,
            initialized: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    pub fn filled(&self) -> &[u8] {
        // SAFETY: filled <= initialized
        unsafe { &*(&self.buf[..self.filled] as *const [MaybeUninit<u8>] as *const [u8]) }
    }

    pub fn filled_mut(&mut self) -> &mut [u8] {
        // SAFETY: filled <= initialized
        unsafe { &mut *(&mut self.buf[..self.filled] as *mut [MaybeUninit<u8>] as *mut [u8]) }
    }

    #[track_caller]
    pub fn take(&mut self, n: usize) -> ReadBuf<'_> {
        let max = std::cmp::min(self.remaining(), n);
        // SAFETY: 未初期化を初期化済みとして見せない（initialized を引き継がない）
        let buf = unsafe { &mut self.unfilled_mut()[..max] };
        ReadBuf::uninit(buf)
    }

    pub fn initialized(&self) -> &[u8] {
        // SAFETY: initialized までは初期化済み
        unsafe { &*(&self.buf[..self.initialized] as *const [MaybeUninit<u8>] as *const [u8]) }
    }

    pub fn initialized_mut(&mut self) -> &mut [u8] {
        // SAFETY: initialized までは初期化済み
        unsafe { &mut *(&mut self.buf[..self.initialized] as *mut [MaybeUninit<u8>] as *mut [u8]) }
    }

    /// # Safety
    /// The caller must not write uninitialised bytes back into the returned slice.
    pub unsafe fn inner_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        self.buf
    }

    /// # Safety
    /// The caller must not write uninitialised bytes back into the returned slice.
    pub unsafe fn unfilled_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        &mut self.buf[self.filled..]
    }

    #[track_caller]
    pub fn initialize_unfilled(&mut self) -> &mut [u8] {
        self.initialize_unfilled_to(self.remaining())
    }

    #[track_caller]
    pub fn initialize_unfilled_to(&mut self, n: usize) -> &mut [u8] {
        assert!(self.remaining() >= n, "n overflows remaining");
        let end = self.filled + n;
        if self.initialized < end {
            for b in &mut self.buf[self.initialized..end] {
                *b = MaybeUninit::new(0);
            }
            self.initialized = end;
        }
        let slice = &mut self.buf[self.filled..end];
        // SAFETY: 直前で初期化した
        unsafe { &mut *(slice as *mut [MaybeUninit<u8>] as *mut [u8]) }
    }

    pub fn remaining(&self) -> usize {
        self.capacity() - self.filled
    }

    pub fn clear(&mut self) {
        self.filled = 0;
    }

    #[track_caller]
    pub fn advance(&mut self, n: usize) {
        let new = self.filled.checked_add(n).expect("filled overflow");
        self.set_filled(new);
    }

    #[track_caller]
    pub fn set_filled(&mut self, n: usize) {
        assert!(
            n <= self.initialized,
            "filled must not become larger than initialized"
        );
        self.filled = n;
    }

    /// # Safety
    /// The bytes up to `filled + n` must really be initialised.
    pub unsafe fn assume_init(&mut self, n: usize) {
        let new = self.filled + n;
        if new > self.initialized {
            self.initialized = new;
        }
    }

    #[track_caller]
    pub fn put_slice(&mut self, buf: &[u8]) {
        assert!(
            self.remaining() >= buf.len(),
            "buf.len() must fit in remaining(); buf.len() = {}, remaining() = {}",
            buf.len(),
            self.remaining()
        );
        let amt = buf.len();
        let end = self.filled + amt;
        // SAFETY: 範囲内へのコピー
        unsafe {
            self.buf[self.filled..end]
                .as_mut_ptr()
                .cast::<u8>()
                .copy_from_nonoverlapping(buf.as_ptr(), amt);
        }
        if self.initialized < end {
            self.initialized = end;
        }
        self.filled = end;
    }
}

// SAFETY: tokio と同じ実装（未初期化領域を UninitSlice として見せ、advance_mut で初期化済み・読み込み済みを進める）
unsafe impl bytes::BufMut for ReadBuf<'_> {
    fn remaining_mut(&self) -> usize {
        self.remaining()
    }

    unsafe fn advance_mut(&mut self, cnt: usize) {
        // SAFETY: 呼び出し側が cnt バイトを初期化した
        unsafe { self.assume_init(cnt) };
        self.advance(cnt);
    }

    fn chunk_mut(&mut self) -> &mut bytes::buf::UninitSlice {
        // SAFETY: 未初期化を初期化済みとして読まない（UninitSlice は書き込み専用）
        let unfilled = unsafe { self.unfilled_mut() };
        let len = unfilled.len();
        let ptr = unfilled.as_mut_ptr() as *mut u8;
        unsafe { bytes::buf::UninitSlice::from_raw_parts_mut(ptr, len) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_buf_semantics() {
        let mut storage = [0u8; 8];
        let mut b = ReadBuf::new(&mut storage);
        assert_eq!(b.remaining(), 8);
        b.put_slice(b"abc");
        assert_eq!(b.filled(), b"abc");
        b.initialize_unfilled_to(2)[..2].copy_from_slice(b"de");
        b.advance(2);
        assert_eq!(b.filled(), b"abcde");
        let mut raw = [MaybeUninit::<u8>::uninit(); 4];
        let mut u = ReadBuf::uninit(&mut raw);
        assert_eq!(u.initialized().len(), 0);
        u.put_slice(b"xy");
        assert_eq!(u.filled(), b"xy");
    }
}
