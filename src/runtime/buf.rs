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

// ====================
// bytes::BytesMut の実装（書き込み可能・プール共有のゼロコピー受信バッファ）
// ====================
//
// F-165 R4: バックエンドから読み取ったボディを `Bytes::copy_from_slice` で都度
// コピーするのではなく、プール済み `BytesMut`（`pool.rs` の `body_buf_get`/
// `body_buf_put`）の空き容量（spare capacity）へ直接読み込み、
// `split_to(n).freeze()` で参照カウント共有のまま切り出して使う。
//
// `Vec<u8>` 実装（`write_ptr` が常に先頭固定・`bytes_total` が `capacity()` 全体）
// とは異なり、`BytesMut` の書き込み先は「呼び出し時点の `len()` を起点とした
// 未初期化領域」である。`AsyncReadRent::read` は 1 回の read 操作につき
// `write_ptr()`/`bytes_total()` を読み込み前に 1 回、`set_init(pos)` を完了後に
// 1 回だけ呼ぶ契約（`src/runtime/io.rs`・各バックエンドの完了ハンドラを参照）
// なので、その間に `len()` が外部から変化しないことが安全性の前提になる。
unsafe impl IoBufMut for bytes::BytesMut {
    #[inline(always)]
    fn write_ptr(&mut self) -> *mut u8 {
        // SAFETY: `spare_capacity_mut()` は `len()..capacity()` の未初期化領域を
        // 指す有効なポインタを返す（bytes クレート自身の不変条件）。
        self.spare_capacity_mut().as_mut_ptr() as *mut u8
    }

    #[inline(always)]
    fn bytes_total(&mut self) -> usize {
        self.spare_capacity_mut().len()
    }

    #[inline(always)]
    unsafe fn set_init(&mut self, pos: usize) {
        // SAFETY: 呼び出し側（read 完了ハンドラ）は `pos` バイト分が
        // `write_ptr()`（= この read 開始時点の spare capacity 先頭）から
        // 書き込み済みであることを `IoBufMut::set_init` の契約として保証する。
        // また `pos <= bytes_total()`（= その時点の spare capacity）も契約上
        // 保証されるため、`len() + pos <= capacity()` となり `set_len` は
        // 確保済み範囲内に収まる。
        let new_len = self.len() + pos;
        unsafe {
            self.set_len(new_len);
        }
    }
}

#[cfg(test)]
mod bytes_mut_io_buf_mut_tests {
    use super::*;
    use bytes::BytesMut;

    #[test]
    fn write_ptr_starts_at_spare_capacity() {
        let mut buf = BytesMut::with_capacity(16);
        buf.extend_from_slice(&[1, 2, 3, 4]);
        let expected_ptr = unsafe { buf.as_ptr().add(4) };
        assert_eq!(IoBufMut::write_ptr(&mut buf) as *const u8, expected_ptr);
        assert_eq!(IoBufMut::bytes_total(&mut buf), 16 - 4);
    }

    #[test]
    fn set_init_extends_len_from_current_position() {
        let mut buf = BytesMut::with_capacity(16);
        buf.extend_from_slice(&[1, 2, 3, 4]);
        unsafe {
            let ptr = IoBufMut::write_ptr(&mut buf);
            std::ptr::write(ptr, 9);
            std::ptr::write(ptr.add(1), 8);
            IoBufMut::set_init(&mut buf, 2);
        }
        assert_eq!(&buf[..], &[1, 2, 3, 4, 9, 8]);
    }

    #[test]
    fn split_to_freeze_leaves_disjoint_writable_region() {
        // split_to().freeze() で切り出した Bytes と、残りの BytesMut への
        // その後の書き込みが同じメモリ領域を指さない（ゼロコピー切り出しの
        // 安全性の核心）ことを検証する。
        let mut buf = BytesMut::with_capacity(16);
        buf.extend_from_slice(b"abcd");
        let frozen = buf.split_to(4).freeze();
        assert_eq!(&frozen[..], b"abcd");

        // 残った buf（len=0, capacity=12）へ新しいデータを書き込む。
        buf.extend_from_slice(b"EFGH");
        // 先に切り出した frozen の内容が上書きされていないことを確認する。
        assert_eq!(&frozen[..], b"abcd");
        assert_eq!(&buf[..], b"EFGH");
    }
}

// ====================
// OffsetBufMut: 読み込み継続用のオフセット付き所有権ビュー
// ====================

/// `Vec<u8>` の `offset` 以降だけを書き込み先として見せる所有権ビュー（F-157）。
///
/// `AsyncReadRent` は所有権ムーブ型の `IoBufMut` を要求するため `&mut [u8]` を
/// 直接渡すことはできない。従来の HTTP/2 受信経路（`fill_read_buf`）は
/// `Vec::split_off(offset)` で末尾を新しいバッファへ確保・コピーして渡し、読み込み
/// 完了後に `extend_from_slice` で結合し直していたが、これは 1 回の read のたびに
/// malloc + memcpy を発生させていた。このラッパーは `Vec<u8>` の所有権をそのまま
/// 保持しつつ `offset` 位置から書き込ませることで、そのコピーを完全に排除する。
///
/// # 不変条件
/// - `offset <= buf.capacity()`（`new()` で `debug_assert!` する）。
/// - `write_ptr()`/`bytes_total()` は `offset` を起点としたビューを返す
///   （読み込み先は `offset..capacity` の未初期化領域でよい）。
/// - `set_init(pos)` は元の `Vec` の `len` を `offset + pos` に設定する。既存の
///   `Vec<u8>` 実装と同じ grow-only（`pos` が小さくても `len` を縮めない）ため、
///   呼び出し側は返却された `Vec` の `len()` ではなく実際に読み込んだバイト数
///   （read の戻り値）だけを有効データ長として扱うこと。
pub struct OffsetBufMut {
    buf: Vec<u8>,
    offset: usize,
}

impl OffsetBufMut {
    #[inline(always)]
    pub fn new(buf: Vec<u8>, offset: usize) -> Self {
        debug_assert!(offset <= buf.capacity());
        Self { buf, offset }
    }

    /// 元の `Vec<u8>` を取り出す。
    #[inline(always)]
    pub fn into_inner(self) -> Vec<u8> {
        self.buf
    }
}

unsafe impl IoBufMut for OffsetBufMut {
    #[inline(always)]
    fn write_ptr(&mut self) -> *mut u8 {
        // SAFETY: `offset <= capacity` は `new()` の不変条件で保証されている。
        // `capacity` 分のメモリは `Vec` が確保済みのため、`offset` だけ進めた
        // ポインタも確保済みメモリ範囲内を指す。
        unsafe { self.buf.as_mut_ptr().add(self.offset) }
    }

    #[inline(always)]
    fn bytes_total(&mut self) -> usize {
        // `len` ではなく `capacity` を使う。読み込み先は未初期化領域でよい。
        self.buf.capacity() - self.offset
    }

    #[inline(always)]
    unsafe fn set_init(&mut self, pos: usize) {
        // SAFETY: 呼び出し側（read 完了ハンドラ）は `pos` バイト分が
        // `write_ptr()` から書き込み済みであることを `IoBufMut::set_init` の
        // 契約として保証する。`offset + pos <= offset + bytes_total() <=
        // capacity()` のため `set_len` の引数は確保済み範囲内に収まる。
        let new_len = self.offset + pos;
        if new_len > self.buf.len() {
            self.buf.set_len(new_len);
        }
    }
}

#[cfg(test)]
mod offset_buf_mut_tests {
    use super::*;

    #[test]
    fn write_ptr_starts_at_offset() {
        let buf = vec![0xAAu8; 16];
        let offset = 4;
        let mut view = OffsetBufMut::new(buf, offset);
        let expected_ptr = unsafe { view.buf.as_ptr().add(offset) };
        assert_eq!(view.write_ptr() as *const u8, expected_ptr);
        assert_eq!(view.bytes_total(), 16 - offset);
    }

    #[test]
    fn into_inner_returns_original_buffer_with_offset_data() {
        let mut buf = Vec::with_capacity(16);
        buf.extend_from_slice(&[1, 2, 3, 4]);
        let offset = buf.len();
        let mut view = OffsetBufMut::new(buf, offset);

        // offset 位置へ書き込む。
        unsafe {
            std::ptr::write(view.write_ptr(), 9);
            view.set_init(1);
        }

        let restored = view.into_inner();
        assert_eq!(restored.len(), offset + 1);
        assert_eq!(&restored[..], &[1, 2, 3, 4, 9]);
    }

    #[test]
    fn set_init_is_grow_only() {
        let mut buf = Vec::with_capacity(16);
        buf.extend_from_slice(&[1, 2, 3, 4]);
        let mut view = OffsetBufMut::new(buf, 0);

        // 一度 len を進める。
        unsafe {
            view.set_init(10);
        }
        assert_eq!(view.buf.len(), 10);

        // より小さい pos を渡しても縮まらない（grow-only）。
        unsafe {
            view.set_init(2);
        }
        assert_eq!(view.buf.len(), 10);
    }
}
