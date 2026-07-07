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
// SlicedIoBuf: 部分書き込み継続用のオフセット付きラッパー
// ====================

/// 部分書き込みの継続用に、内部バッファの `offset` 以降だけを公開する `IoBuf` ラッパー。
///
/// `write_all`（`src/runtime/io.rs`）が short write の残りを**追加アロケーションなし**で
/// 書き続けるために使う（B-27）。`advance()` で送信済みバイト数を進め、完了後は
/// `into_inner()` で元のバッファを取り出して呼び出し側へ返却する。
pub struct SlicedIoBuf<T: IoBuf> {
    inner: T,
    offset: usize,
}

impl<T: IoBuf> SlicedIoBuf<T> {
    #[inline(always)]
    pub fn new(inner: T) -> Self {
        Self { inner, offset: 0 }
    }

    /// 送信済みバイト数を進める（`bytes_init` を超えないよう飽和）。
    #[inline(always)]
    pub fn advance(&mut self, n: usize) {
        self.offset = (self.offset + n).min(self.inner.bytes_init());
    }

    /// 元のバッファを取り出す。
    #[inline(always)]
    pub fn into_inner(self) -> T {
        self.inner
    }
}

unsafe impl<T: IoBuf> IoBuf for SlicedIoBuf<T> {
    #[inline(always)]
    fn read_ptr(&self) -> *const u8 {
        // SAFETY: offset は常に bytes_init 以下（advance で飽和）のため範囲内。
        unsafe { self.inner.read_ptr().add(self.offset) }
    }

    #[inline(always)]
    fn bytes_init(&self) -> usize {
        self.inner.bytes_init() - self.offset
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

// ====================
// bytes::Bytes の実装（読み取り専用・参照カウント共有のゼロコピーバッファ）
// ====================
//
// `Bytes` は内部で確保済みバッファへの参照カウントを持つ不変ビューであり、
// `clone()` は O(1)（refcount +1）でデータをコピーしない。`WriteFuture` に所有権を
// 渡すと in-flight 中はバッファが生存し続け、ドロップ時も B-07 のガードで CQE 到着まで
// 保持される。これによりキャッシュヒットのボディを memcpy なしでソケットへ送出できる。
unsafe impl IoBuf for bytes::Bytes {
    #[inline(always)]
    fn read_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    #[inline(always)]
    fn bytes_init(&self) -> usize {
        self.len()
    }
}
