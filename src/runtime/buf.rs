//! IoBuf / IoBufMut トレイト定義
//!
//! monoio の同名トレイトを自前で定義する。
//! io_uring の所有権ベース I/O でバッファを安全に渡すための抽象化。

/// 読み取り専用バッファトレイト（io_uring への渡し用）
///
/// # Safety
/// - `read_ptr()` は有効なメモリへのポインタを返すこと
/// - `bytes_init()` は初期化済みバイト数を返すこと
pub unsafe trait IoBuf: 'static {
    /// バッファの先頭ポインタ
    fn read_ptr(&self) -> *const u8;

    /// 初期化済みバイト数
    fn bytes_init(&self) -> usize;
}

/// 書き込み可能バッファトレイト（io_uring からの受け取り用）
///
/// # Safety
/// - `write_ptr()` は書き込み可能な有効なメモリへのポインタを返すこと
/// - `bytes_total()` はバッファの総容量を返すこと
/// - `set_init(pos)` は `pos` バイトが初期化済みであることを記録すること
pub unsafe trait IoBufMut: 'static {
    /// バッファの先頭可変ポインタ
    fn write_ptr(&mut self) -> *mut u8;

    /// バッファの総容量
    fn bytes_total(&mut self) -> usize;

    /// 初期化済みバイト数を設定
    ///
    /// # Safety
    /// `pos` バイトまでが有効なデータで埋まっていること
    unsafe fn set_init(&mut self, pos: usize);
}

// ====================
// Vec<u8> の実装
// ====================

unsafe impl IoBuf for Vec<u8> {
    #[inline(always)]
    fn read_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    #[inline(always)]
    fn bytes_init(&self) -> usize {
        self.len()
    }
}

unsafe impl IoBufMut for Vec<u8> {
    #[inline(always)]
    fn write_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    #[inline(always)]
    fn bytes_total(&mut self) -> usize {
        self.capacity()
    }

    #[inline(always)]
    unsafe fn set_init(&mut self, pos: usize) {
        if pos > self.len() {
            self.set_len(pos);
        }
    }
}

// ====================
// Box<[u8]> の実装
// ====================

unsafe impl IoBuf for Box<[u8]> {
    #[inline(always)]
    fn read_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    #[inline(always)]
    fn bytes_init(&self) -> usize {
        self.len()
    }
}

unsafe impl IoBufMut for Box<[u8]> {
    #[inline(always)]
    fn write_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    #[inline(always)]
    fn bytes_total(&mut self) -> usize {
        self.len()
    }

    #[inline(always)]
    unsafe fn set_init(&mut self, _pos: usize) {
        // Box<[u8]> は固定長なので何もしない
    }
}
