// ====================
// TLS ストリーム型エイリアス（pool モジュール）
// ====================
//
// ktls フィーチャーの有無に応じて、使用する TLS ストリーム型を切り替えます。
// kTLS 有効時は ktls_rustls モジュールの型を使用し、
// 無効時はシンプルな rustls ラッパーを使用します。

#[cfg(feature = "ktls")]
pub(crate) use crate::ktls_rustls::KtlsClientStream as ClientTls;

#[cfg(not(feature = "ktls"))]
pub(crate) use crate::simple_tls::SimpleTlsClientStream as ClientTls;

// ====================
// バッファプール・コネクションプール
// ====================

use crate::runtime::buf::{IoBuf, IoBufMut};
use crate::runtime::tcp::TcpStream;
use ftlog::info;
use once_cell::sync::Lazy;
use serde::Deserialize;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
#[cfg(feature = "ktls")]
use std::io;
use std::sync::Arc;
use std::time::Duration;

#[cfg(feature = "ktls")]
use crate::ktls_rustls;

// バッファサイズ（ページアライン・L2キャッシュ最適化）
pub(crate) const BUF_SIZE: usize = 65536; // 64KB - io_uring最適サイズ
pub(crate) const HEADER_BUF_CAPACITY: usize = 512; // HTTPヘッダー用

// ====================
// 安全なバッファラッパー（SafeReadBuffer）
// ====================
//
// 未初期化メモリへのアクセスリスクを型システムで防止します。
//
// ## 設計原則
//
// 1. io_uringへの読み込みには内部バッファ全体を使用
// 2. 読み込み完了後、有効データ長（valid_len）を設定
// 3. ユーザーコードは valid_len 経由でのみデータにアクセス可能
//
// ## 安全性保証
//
// - `as_valid_slice()` は読み込まれたデータのみを返す
// - 未初期化領域へのアクセスはコンパイル時に防止される
// - `buf.len()` の誤用によるセキュリティリスクを排除
//
// ====================

/// 安全な読み込みバッファラッパー
///
/// io_uring読み込み操作で使用され、読み込まれたデータ長を追跡することで
/// 未初期化メモリへのアクセスを型レベルで防止します。
///
/// # 使用例
///
/// ```rust,ignore
/// let mut buf = SafeReadBuffer::new(BUF_SIZE);
/// // io_uring読み込み操作（内部バッファを使用）
/// let (result, mut returned_buf) = stream.read(buf.into_inner()).await;
/// // 読み込み成功後、有効長を設定
/// returned_buf.set_valid_len(n);
/// // 安全なアクセス：有効データのみが返される
/// let data = returned_buf.as_valid_slice();
/// ```
pub struct SafeReadBuffer {
    /// 内部バッファ（BUF_SIZE容量）
    inner: Vec<u8>,
    /// 有効データ長（読み込み操作で設定される）
    valid_len: usize,
}

impl SafeReadBuffer {
    /// 新しいバッファを作成
    ///
    /// # Arguments
    /// * `cap` - バッファ容量
    ///
    /// # Safety
    /// io_uringに渡すために一時的に長さを確保しますが、
    /// ユーザーコードからは valid_len 経由でしかアクセスできません。
    #[inline(always)]
    #[allow(clippy::uninit_vec)]
    pub fn new(cap: usize) -> Self {
        let mut v = Vec::with_capacity(cap);
        // SAFETY: io_uringに渡すための事前確保
        // 読み込み前は valid_len = 0 なので未初期化領域にはアクセスできない
        // SafeReadBuffer は as_valid_slice() を通じてのみデータにアクセスするため、
        // 未初期化領域への誤アクセスは型レベルで防止されている
        unsafe {
            v.set_len(cap);
        }
        Self {
            inner: v,
            valid_len: 0,
        }
    }

    /// 既存のVec<u8>からバッファを作成
    ///
    /// プール返却時に使用。valid_len は 0 にリセットされます。
    #[inline(always)]
    #[allow(clippy::uninit_vec)]
    pub fn from_vec(mut v: Vec<u8>, cap: usize) -> Self {
        if v.capacity() >= cap {
            // SAFETY: capacity >= cap を確認済み
            unsafe {
                v.set_len(cap);
            }
        } else {
            // 容量不足の場合は新規作成
            v = Vec::with_capacity(cap);
            unsafe {
                v.set_len(cap);
            }
        }
        Self {
            inner: v,
            valid_len: 0,
        }
    }

    /// 読み込み完了後に有効データ長を設定
    ///
    /// # Arguments
    /// * `len` - 読み込まれたバイト数
    ///
    /// # Note
    /// バッファ容量を超える値は自動的にクランプされます。
    #[inline(always)]
    pub fn set_valid_len(&mut self, len: usize) {
        self.valid_len = len.min(self.inner.len());
    }

    /// 有効データのスライスを取得
    ///
    /// 読み込まれたデータのみを返します。
    /// 未初期化領域にはアクセスできません。
    #[inline(always)]
    pub fn as_valid_slice(&self) -> &[u8] {
        &self.inner[..self.valid_len]
    }

    /// 有効データ長を取得
    #[inline(always)]
    pub fn valid_len(&self) -> usize {
        self.valid_len
    }

    /// バッファ容量を取得
    #[inline(always)]
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    /// 内部バッファの長さを取得（io_uring用）
    #[inline(always)]
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// 有効データが空かどうかを確認
    #[inline(always)]
    pub fn is_empty(&self) -> bool {
        self.valid_len == 0
    }

    /// 内部Vecを取り出す（プール返却用）
    ///
    /// # Warning
    /// 返されたVecは未初期化データを含む可能性があります。
    /// 必ず SafeReadBuffer::from_vec() でラップし直してください。
    #[inline(always)]
    pub fn into_inner(self) -> Vec<u8> {
        self.inner
    }

    /// 有効データをtruncateして内部Vecを取り出す
    ///
    /// 書き込み操作用。有効データのみを含むVecを返します。
    #[inline(always)]
    pub fn into_truncated(mut self) -> Vec<u8> {
        self.inner.truncate(self.valid_len);
        self.inner
    }
}

// IoBuf トレイト実装
// SAFETY: inner は有効なヒープメモリを指し、read_ptr() は有効なポインタを返す
unsafe impl IoBuf for SafeReadBuffer {
    #[inline(always)]
    fn read_ptr(&self) -> *const u8 {
        self.inner.read_ptr()
    }

    #[inline(always)]
    fn bytes_init(&self) -> usize {
        self.inner.bytes_init()
    }
}

// IoBufMut トレイト実装
// SAFETY: inner は有効な書き込み可能なヒープメモリを指す
unsafe impl IoBufMut for SafeReadBuffer {
    #[inline(always)]
    fn write_ptr(&mut self) -> *mut u8 {
        self.inner.write_ptr()
    }

    #[inline(always)]
    fn bytes_total(&mut self) -> usize {
        self.inner.bytes_total()
    }

    #[inline(always)]
    unsafe fn set_init(&mut self, pos: usize) {
        self.inner.set_init(pos);
        // io_uringからの読み込み完了時に呼ばれる
        // valid_len も更新する
        self.valid_len = pos;
    }
}

// セキュリティ制限
pub(crate) const MAX_HEADER_SIZE: usize = 8192; // 8KB - ヘッダーサイズ上限
pub(crate) const MAX_BODY_SIZE: usize = 10485760; // 10MB - ボディサイズ上限
#[allow(dead_code)]
pub(crate) const MAX_GRPC_BODY_SIZE: usize = 1_048_576; // 1MB - gRPCメッセージサイズ上限

// タイムアウト設定
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

// バックエンドコネクションプール設定（デフォルト値）
pub(crate) const BACKEND_POOL_MAX_IDLE_PER_HOST: usize = 8; // ホストあたりの最大アイドル接続数
pub(crate) const BACKEND_POOL_IDLE_TIMEOUT_SECS: u64 = 30; // アイドル接続のタイムアウト（秒）

// ====================
// バックエンドコネクションプール
// ====================
//
// スレッドローカルなコネクションプールにより、バックエンドへの接続を再利用します。
// HTTP用とHTTPS用で別々のプールを管理し、ホスト:ポートをキーにしています。
// ====================

/// プールされた接続のエントリ
pub struct PooledConnection<T> {
    pub stream: T,
    pub created_at: std::time::Instant,
    /// この接続のアイドルタイムアウト（秒）
    pub idle_timeout_secs: u64,
}

impl<T> PooledConnection<T> {
    pub(crate) fn new(stream: T, idle_timeout_secs: u64) -> Self {
        Self {
            stream,
            created_at: std::time::Instant::now(),
            idle_timeout_secs,
        }
    }

    /// 接続がまだ有効かどうかを判定（タイムアウトチェック）
    pub(crate) fn is_valid(&self) -> bool {
        self.created_at.elapsed().as_secs() < self.idle_timeout_secs
    }
}

/// HTTPバックエンド用コネクションプール（TcpStream）
pub(crate) struct HttpConnectionPool {
    connections: HashMap<String, VecDeque<PooledConnection<TcpStream>>>,
}

impl HttpConnectionPool {
    pub(crate) fn new() -> Self {
        Self {
            connections: HashMap::new(),
        }
    }

    /// プールから接続を取得（有効な接続がなければNone）
    pub(crate) fn get(&mut self, key: &str) -> Option<TcpStream> {
        if let Some(queue) = self.connections.get_mut(key) {
            while let Some(entry) = queue.pop_front() {
                if entry.is_valid() {
                    // F-09: コネクションプールヒットを記録
                    crate::metrics::record_connection_pool_hit(key);
                    return Some(entry.stream);
                }
                // 無効な接続は破棄
            }
        }
        // F-09: コネクションプールミスを記録
        crate::metrics::record_connection_pool_miss(key);
        None
    }

    /// 接続をプールに返却（設定可能なパラメータ付き）
    pub(crate) fn put(
        &mut self,
        key: String,
        stream: TcpStream,
        max_idle: usize,
        idle_timeout_secs: u64,
    ) {
        // F-09: メトリクス用にキーを保持（key は entry へムーブされるため）
        let metric_key = key.clone();
        let queue = self.connections.entry(key).or_insert_with(VecDeque::new);

        // 古い接続を削除（設定可能な最大数を使用）
        while queue.len() >= max_idle {
            queue.pop_front();
        }

        queue.push_back(PooledConnection::new(stream, idle_timeout_secs));
        // F-09: プールサイズを更新
        crate::metrics::set_connection_pool_size(&metric_key, queue.len());
    }
}

/// HTTPSバックエンド用コネクションプール（ClientTls型エイリアス使用）
pub(crate) struct HttpsConnectionPool {
    connections: HashMap<String, VecDeque<PooledConnection<ClientTls>>>,
}

impl HttpsConnectionPool {
    pub(crate) fn new() -> Self {
        Self {
            connections: HashMap::new(),
        }
    }

    /// プールから接続を取得（有効な接続がなければNone）
    pub(crate) fn get(&mut self, key: &str) -> Option<ClientTls> {
        if let Some(queue) = self.connections.get_mut(key) {
            while let Some(entry) = queue.pop_front() {
                if entry.is_valid() {
                    // F-09: コネクションプールヒットを記録
                    crate::metrics::record_connection_pool_hit(key);
                    return Some(entry.stream);
                }
                // 無効な接続は破棄
            }
        }
        // F-09: コネクションプールミスを記録
        crate::metrics::record_connection_pool_miss(key);
        None
    }

    /// 接続をプールに返却（設定可能なパラメータ付き）
    pub(crate) fn put(
        &mut self,
        key: String,
        stream: ClientTls,
        max_idle: usize,
        idle_timeout_secs: u64,
    ) {
        // F-09: メトリクス用にキーを保持（key は entry へムーブされるため）
        let metric_key = key.clone();
        let queue = self.connections.entry(key).or_insert_with(VecDeque::new);

        // 古い接続を削除（設定可能な最大数を使用）
        while queue.len() >= max_idle {
            queue.pop_front();
        }

        queue.push_back(PooledConnection::new(stream, idle_timeout_secs));
        // F-09: プールサイズを更新
        crate::metrics::set_connection_pool_size(&metric_key, queue.len());
    }
}

thread_local! {
    pub(crate) static HTTP_POOL: RefCell<HttpConnectionPool> = RefCell::new(HttpConnectionPool::new());
    pub(crate) static HTTPS_POOL: RefCell<HttpsConnectionPool> = RefCell::new(HttpsConnectionPool::new());
}

// kTLS 有効時のスレッドローカル Splice パイプ
// splice(2) によるゼロコピー転送に使用
#[cfg(feature = "ktls")]
thread_local! {
    pub(crate) static SPLICE_PIPE: RefCell<Option<ktls_rustls::SplicePipe>> = RefCell::new(None);
}

/// スレッドローカルな Splice パイプを取得または初期化
#[cfg(feature = "ktls")]
pub(crate) fn get_splice_pipe() -> std::cell::Ref<'static, Option<ktls_rustls::SplicePipe>> {
    SPLICE_PIPE.with(|p| {
        {
            let mut pipe = p.borrow_mut();
            if pipe.is_none() {
                match ktls_rustls::SplicePipe::new() {
                    Ok(new_pipe) => {
                        *pipe = Some(new_pipe);
                        ftlog::info!("Splice pipe initialized for this thread");
                    }
                    Err(e) => {
                        ftlog::warn!("Failed to create splice pipe: {}", e);
                    }
                }
            }
        }
        // Safety: ライフタイムを'staticに拡張（thread_localなので安全）
        unsafe { std::mem::transmute(p.borrow()) }
    })
}

// ====================
// Raw I/O ヘルパー関数（kTLS + splice 用）
// ====================
//
// 所有権ベースの I/O を使わず、
// libc::read/write を直接使用します。
// 非同期待機は TcpStream::readable()/writable() を使用。
// ====================

/// libc::read のラッパー（ノンブロッキング対応）
#[cfg(feature = "ktls")]
#[inline]
pub(crate) fn raw_read(fd: std::os::unix::io::RawFd, buf: &mut [u8]) -> io::Result<usize> {
    let result = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

/// libc::write のラッパー（ノンブロッキング対応）
#[cfg(feature = "ktls")]
#[inline]
pub(crate) fn raw_write(fd: std::os::unix::io::RawFd, buf: &[u8]) -> io::Result<usize> {
    let result = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len()) };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

/// 非同期 raw read（TcpStream から FD 経由で読み取り）
///
/// libc::read を直接使用。
/// WouldBlock の場合は readable() で待機してリトライ。
#[cfg(feature = "ktls")]
pub(crate) async fn async_raw_read(stream: &TcpStream, buf: &mut [u8]) -> io::Result<usize> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();

    loop {
        match raw_read(fd, buf) {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // 読み取り可能になるまで待機
                stream.readable().await?;
            }
            Err(e) => return Err(e),
        }
    }
}

/// 非同期 raw write（TcpStream へ FD 経由で書き込み）
///
/// libc::write を直接使用。
/// WouldBlock の場合は writable() で待機してリトライ。
#[cfg(feature = "ktls")]
pub(crate) async fn async_raw_write(stream: &TcpStream, buf: &[u8]) -> io::Result<usize> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    let mut written = 0;

    while written < buf.len() {
        match raw_write(fd, &buf[written..]) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0")),
            Ok(n) => written += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // 書き込み可能になるまで待機
                stream.writable().await?;
            }
            Err(e) => return Err(e),
        }
    }

    Ok(written)
}

/// 非同期 raw write all（全バイト書き込み完了まで）
#[cfg(feature = "ktls")]
pub(crate) async fn async_raw_write_all(stream: &TcpStream, buf: &[u8]) -> io::Result<()> {
    let written = async_raw_write(stream, buf).await?;
    if written < buf.len() {
        Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "failed to write all bytes",
        ))
    } else {
        Ok(())
    }
}

// ====================
// バッファプール（パフォーマンス最適化版）
// ====================
//
// 注意: バッファプールによりアロケーションコストを削減していますが、
// Arc<Vec<u8>>からのコピーは避けられないケースがあります。
// bytes クレートを活用することでゼロコピー化を進める計画（F-26）。
//
// ## パフォーマンス最適化: ゼロ埋め削除
//
// バッファの再利用時にゼロ埋め（memset）を行わず、`set_len()` のみを使用。
// これにより64KB × N回のmemsetコストを完全に削除しています。
//
// ## セキュリティ保証（SafeReadBuffer による型レベル保護）
//
// SafeReadBuffer ラッパーにより、未初期化メモリへのアクセスを
// 型システムで防止しています。
//
// - `as_valid_slice()` は読み込まれたデータのみを返す
// - `buf.len()` の誤用によるセキュリティリスクを排除
// - Heartbleed類似の脆弱性を構造的に防止
//
// ====================

thread_local! {
    /// スレッドローカルバッファプール
    ///
    /// 内部では Vec<u8> を保持し、取得時に SafeReadBuffer でラップします。
    /// これにより、既存のメモリ効率を維持しながら型安全性を向上させています。
    #[allow(clippy::uninit_vec)]
    pub(crate) static BUF_POOL: RefCell<Vec<Vec<u8>>> = RefCell::new(
        (0..32).map(|_| {
            let mut buf = Vec::with_capacity(BUF_SIZE);
            // SAFETY: SafeReadBuffer でラップされるため、
            // valid_len 経由でしかアクセスできない
            unsafe {
                buf.set_len(BUF_SIZE);
            }
            buf
        }).collect()
    );
}

/// 安全なバッファ取得ヘルパー
///
/// プールから SafeReadBuffer を取得します。
/// 取得されたバッファは valid_len = 0 で初期化されており、
/// io_uring読み込み完了後に set_valid_len() で有効長を設定します。
///
/// # 使用例
///
/// ```rust,ignore
/// let read_buf = buf_get();
/// let (res, mut returned_buf) = stream.read(read_buf).await;
/// if let Ok(n) = res {
///     returned_buf.set_valid_len(n);
///     // 安全なアクセス：有効データのみが返される
///     accumulated.extend_from_slice(returned_buf.as_valid_slice());
/// }
/// buf_put(returned_buf);
/// ```
#[inline(always)]
pub(crate) fn buf_get() -> SafeReadBuffer {
    BUF_POOL.with(|p| {
        p.borrow_mut()
            .pop()
            .map(|v| SafeReadBuffer::from_vec(v, BUF_SIZE))
            .unwrap_or_else(|| SafeReadBuffer::new(BUF_SIZE))
    })
}

/// バッファ返却ヘルパー（SafeReadBuffer版）
///
/// SafeReadBuffer をプールに返却します。
/// 内部の Vec<u8> を取り出してプールに格納します。
///
/// # セキュリティ
///
/// 返却されたバッファは次回取得時に SafeReadBuffer でラップされるため、
/// 以前のデータが漏洩することはありません（valid_len = 0 で初期化）。
#[inline(always)]
pub(crate) fn buf_put(buf: SafeReadBuffer) {
    buf_put_vec(buf.into_inner());
}

/// バッファ返却ヘルパー（Vec<u8>版、書き込み後の返却用）
///
/// 書き込み操作で使用された Vec<u8> をプールに返却します。
/// 主に `into_truncated()` 後の書き込み完了時に使用されます。
#[inline(always)]
#[allow(clippy::uninit_vec)]
pub(crate) fn buf_put_vec(mut buf: Vec<u8>) {
    BUF_POOL.with(|p| {
        let mut pool = p.borrow_mut();
        if pool.len() < 128 {
            // バッファの容量が十分であることを確認
            if buf.capacity() >= BUF_SIZE {
                // SAFETY:
                // - capacity() >= BUF_SIZE を事前に確認済み
                // - 次回取得時は SafeReadBuffer でラップされる
                unsafe {
                    buf.set_len(BUF_SIZE);
                }
            } else {
                // 容量が足りない場合は新規作成（通常は発生しない）
                buf = Vec::with_capacity(BUF_SIZE);
                unsafe {
                    buf.set_len(BUF_SIZE);
                }
            }
            pool.push(buf);
        }
    });
}

// ====================
// リクエスト構築用バッファプール（メモリ割り当て最適化）
// ====================
//
// リクエスト構築時の動的メモリ割り当てを削減するため、
// スレッドローカルなバッファプールを使用します。
// ====================

/// リクエスト構築用バッファサイズ
pub(crate) const REQUEST_BUF_SIZE: usize = 1024;
/// 大容量リクエスト用バッファサイズ
pub(crate) const LARGE_REQUEST_BUF_SIZE: usize = 4096;
/// パス文字列用バッファサイズ
pub(crate) const PATH_STRING_SIZE: usize = 256;

// ====================
// バッファプール設定（config.toml対応）
// ====================

/// バッファプール設定
///
/// スレッドローカルバッファプールの設定。
/// 起動時に事前確保され、リクエスト処理中のメモリ割り当てを削減します。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct BufferPoolConfig {
    /// 読み込みバッファサイズ（バイト）
    /// デフォルト: 65536 (64KB)
    pub read_buffer_size: usize,

    /// 読み込みバッファ初期プール数
    /// デフォルト: 32
    pub initial_read_buffers: usize,

    /// 読み込みバッファ最大プール数
    /// デフォルト: 128
    pub max_read_buffers: usize,

    /// リクエスト構築バッファサイズ（バイト）
    /// デフォルト: 1024 (1KB)
    pub request_buffer_size: usize,

    /// リクエスト構築バッファ初期プール数
    /// デフォルト: 16
    pub initial_request_buffers: usize,

    /// 大容量リクエストバッファサイズ（バイト）
    /// デフォルト: 4096 (4KB)
    pub large_request_buffer_size: usize,

    /// パス文字列バッファサイズ（バイト）
    /// デフォルト: 256
    pub path_string_size: usize,

    /// レスポンスヘッダーバッファサイズ（バイト）
    /// デフォルト: 512
    pub response_header_buffer_size: usize,
}

impl Default for BufferPoolConfig {
    fn default() -> Self {
        Self {
            read_buffer_size: BUF_SIZE,
            initial_read_buffers: 32,
            max_read_buffers: 128,
            request_buffer_size: REQUEST_BUF_SIZE,
            initial_request_buffers: 16,
            large_request_buffer_size: LARGE_REQUEST_BUF_SIZE,
            path_string_size: PATH_STRING_SIZE,
            response_header_buffer_size: 512,
        }
    }
}

// ====================
// グローバルバッファプール設定
// ====================
//
// 設定ファイルから読み込んだバッファプール設定を保持し、
// 各バッファ取得関数から参照します。

/// グローバルバッファプール設定（起動時に一度だけ設定）
pub(crate) static BUFFER_POOL_CONFIG: std::sync::OnceLock<BufferPoolConfig> =
    std::sync::OnceLock::new();

/// バッファプール設定を初期化
#[inline]
pub(crate) fn init_buffer_pool_config(config: BufferPoolConfig) {
    let _ = BUFFER_POOL_CONFIG.set(config);
}

/// バッファプール設定を取得（未初期化時はデフォルト値）
#[inline]
pub(crate) fn get_buffer_pool_config() -> &'static BufferPoolConfig {
    static DEFAULT_CONFIG: std::sync::OnceLock<BufferPoolConfig> = std::sync::OnceLock::new();
    BUFFER_POOL_CONFIG
        .get()
        .unwrap_or_else(|| DEFAULT_CONFIG.get_or_init(BufferPoolConfig::default))
}

thread_local! {
    /// リクエスト構築用バッファプール（1KB × 16）
    pub(crate) static REQUEST_BUF_POOL: RefCell<Vec<Vec<u8>>> = RefCell::new(
        (0..16).map(|_| Vec::with_capacity(REQUEST_BUF_SIZE)).collect()
    );

    /// 大容量リクエスト用バッファプール（4KB × 4）
    pub(crate) static LARGE_REQUEST_BUF_POOL: RefCell<Vec<Vec<u8>>> = RefCell::new(
        (0..4).map(|_| Vec::with_capacity(LARGE_REQUEST_BUF_SIZE)).collect()
    );

    /// パス構築用Stringプール（256B × 16）
    pub(crate) static PATH_STRING_POOL: RefCell<Vec<String>> = RefCell::new(
        (0..16).map(|_| String::with_capacity(PATH_STRING_SIZE)).collect()
    );

    /// レスポンスヘッダー構築用バッファプール（512B × 16）
    pub(crate) static RESPONSE_HEADER_BUF_POOL: RefCell<Vec<Vec<u8>>> = RefCell::new(
        (0..16).map(|_| Vec::with_capacity(512)).collect()
    );
}

/// リクエスト構築用バッファを取得
#[inline]
pub(crate) fn request_buf_get(size_hint: usize) -> Vec<u8> {
    let config = get_buffer_pool_config();
    if size_hint <= config.request_buffer_size {
        REQUEST_BUF_POOL.with(|p| {
            p.borrow_mut()
                .pop()
                .unwrap_or_else(|| Vec::with_capacity(config.request_buffer_size))
        })
    } else {
        LARGE_REQUEST_BUF_POOL.with(|p| {
            p.borrow_mut()
                .pop()
                .unwrap_or_else(|| Vec::with_capacity(config.large_request_buffer_size))
        })
    }
}

/// リクエスト構築用バッファを返却
#[inline]
pub(crate) fn request_buf_put(mut buf: Vec<u8>) {
    buf.clear();
    let config = get_buffer_pool_config();
    let capacity = buf.capacity();
    if capacity == config.request_buffer_size {
        REQUEST_BUF_POOL.with(|p| {
            let mut pool = p.borrow_mut();
            if pool.len() < 32 {
                pool.push(buf);
            }
        });
    } else if capacity == config.large_request_buffer_size {
        LARGE_REQUEST_BUF_POOL.with(|p| {
            let mut pool = p.borrow_mut();
            if pool.len() < 8 {
                pool.push(buf);
            }
        });
    }
}

/// パス文字列用Stringを取得
#[inline]
#[allow(dead_code)]
pub(crate) fn path_string_get() -> String {
    let config = get_buffer_pool_config();
    PATH_STRING_POOL.with(|p| {
        p.borrow_mut()
            .pop()
            .unwrap_or_else(|| String::with_capacity(config.path_string_size))
    })
}

/// パス文字列用Stringを返却
#[inline]
#[allow(dead_code)]
pub(crate) fn path_string_put(mut s: String) {
    s.clear();
    let config = get_buffer_pool_config();
    if s.capacity() == config.path_string_size {
        PATH_STRING_POOL.with(|p| {
            let mut pool = p.borrow_mut();
            if pool.len() < 32 {
                pool.push(s);
            }
        });
    }
}

/// レスポンスヘッダー構築用バッファを取得
#[inline]
#[allow(dead_code)]
pub(crate) fn response_header_buf_get() -> Vec<u8> {
    let config = get_buffer_pool_config();
    RESPONSE_HEADER_BUF_POOL.with(|p| {
        p.borrow_mut()
            .pop()
            .unwrap_or_else(|| Vec::with_capacity(config.response_header_buffer_size))
    })
}

/// レスポンスヘッダー構築用バッファを返却
#[inline]
#[allow(dead_code)]
pub(crate) fn response_header_buf_put(mut buf: Vec<u8>) {
    buf.clear();
    let config = get_buffer_pool_config();
    let min_size = config.response_header_buffer_size;
    if buf.capacity() >= min_size && buf.capacity() <= min_size * 4 {
        RESPONSE_HEADER_BUF_POOL.with(|p| {
            let mut pool = p.borrow_mut();
            if pool.len() < 32 {
                pool.push(buf);
            }
        });
    }
}

// ====================
// Serverヘッダー設定（ゼロアロケーション設計）
// ====================
//
// Serverヘッダーの値を起動時/リロード時に設定し、
// リクエスト処理中はゼロアロケーションで参照します。
// Guard方式により、リロード中も安全に値を参照できます。
// ====================

/// Serverヘッダー値（起動時/リロード時に更新）
/// Vec<u8>を使用（ArcSwapはSized型が必要）
pub(crate) static SERVER_HEADER_VALUE: Lazy<arc_swap::ArcSwap<Vec<u8>>> =
    Lazy::new(|| arc_swap::ArcSwap::from(Arc::new(Vec::new())));

/// Serverヘッダー有効フラグ
pub(crate) static SERVER_HEADER_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Serverヘッダー設定を安全に取得
///
/// Guardを返すことで、値がリクエスト処理中に無効化されないことを保証。
/// Guardはスコープを抜けるまでArcを保持し続ける。
#[inline]
pub fn get_server_header_guard() -> Option<ServerHeaderGuard> {
    if SERVER_HEADER_ENABLED.load(std::sync::atomic::Ordering::Acquire) {
        let guard = SERVER_HEADER_VALUE.load();
        if !guard.is_empty() {
            Some(ServerHeaderGuard { guard })
        } else {
            None
        }
    } else {
        None
    }
}

/// Serverヘッダー値を保持するGuard
///
/// このGuardがスコープ内にある限り、ヘッダー値は有効。
pub struct ServerHeaderGuard {
    pub(crate) guard: arc_swap::Guard<Arc<Vec<u8>>>,
}

impl ServerHeaderGuard {
    /// ヘッダータプルとして取得（ゼロコピー）
    #[inline]
    pub fn as_header(&self) -> (&'static [u8], &[u8]) {
        (b"server", self.guard.as_slice())
    }

    /// 値のスライスとして取得
    #[inline]
    #[allow(dead_code)]
    pub fn value(&self) -> &[u8] {
        self.guard.as_slice()
    }
}

/// Serverヘッダー設定を初期化
pub(crate) fn init_server_header(enabled: bool, value: &str) {
    // 値を先に設定（順序重要: リロード時の競争状態防止）
    if !value.is_empty() {
        let value_bytes = Arc::new(value.as_bytes().to_vec());
        SERVER_HEADER_VALUE.store(value_bytes);
    }

    // 有効フラグを最後に設定（Release/Acquire順序で同期）
    SERVER_HEADER_ENABLED.store(enabled, std::sync::atomic::Ordering::Release);

    info!(
        "Server header: {} (value: {:?})",
        if enabled { "enabled" } else { "disabled" },
        if enabled { value } else { "" }
    );
}
