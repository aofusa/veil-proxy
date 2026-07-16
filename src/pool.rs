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
                                                  // http2 (H2C/gRPC) 経路でのみ使用
#[cfg_attr(not(feature = "http2"), allow(dead_code))]
pub(crate) const MAX_GRPC_BODY_SIZE: usize = 1_048_576; // 1MB - gRPCメッセージサイズ上限

// タイムアウト設定
pub(crate) const READ_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

// B-17: バックエンド応答ヘッダー読取の専用タイムアウト。
// 上流が無応答・ヘッダー途中で停止した場合に、クライアントを長時間待たせず
// 速やかに 504 を返すため READ_TIMEOUT より短くする。
pub(crate) const BACKEND_HEADER_TIMEOUT: Duration = Duration::from_secs(10);

// B-17: バックエンド応答ヘッダーサイズの上限。
// 超過時は 502 を返して接続をクローズする（巨大ヘッダーによるメモリ肥大・ハング防止）。
// リクエスト側の MAX_HEADER_SIZE (8KB) より緩く、大きな Set-Cookie 等を許容する。
pub(crate) const MAX_RESPONSE_HEADER_SIZE: usize = 64 * 1024;

// バックエンドコネクションプール設定（デフォルト値）
// B-44: F-116 のストリーム多重化により 1 スレッドあたり同時 ~250 ストリームが
// 独立にバックエンド接続を get/put するようになったため、8 では完了波のたびに
// 超過分がクローズされて TIME_WAIT が蓄積し、エフェメラルポート枯渇（EADDRNOTAVAIL）
// を招いていた。スレッドあたり同時ストリーム数を吸収できる 256 に引き上げる
// （アイドル接続は BACKEND_POOL_IDLE_TIMEOUT_SECS で回収されるため定常的な
// fd 保持は一時的。Envoy の upstream 接続上限既定 1024 と同水準）。
pub(crate) const BACKEND_POOL_MAX_IDLE_PER_HOST: usize = 256; // ホストあたりの最大アイドル接続数
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
        let queue = self.connections.entry(key).or_default();

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
        let queue = self.connections.entry(key).or_default();

        // 古い接続を削除（設定可能な最大数を使用）
        while queue.len() >= max_idle {
            queue.pop_front();
        }

        queue.push_back(PooledConnection::new(stream, idle_timeout_secs));
        // F-09: プールサイズを更新
        crate::metrics::set_connection_pool_size(&metric_key, queue.len());
    }
}

/// H2C（HTTP/2 平文）バックエンド用コネクションプール（F-106）。
///
/// gRPC 中継など `use_h2c` バックエンドへの接続を再利用し、リクエストごとの
/// TCP 接続 + h2c ハンドシェイク（コネクションプリフェース + SETTINGS 往復）を
/// 排除する。HTTP/1.1 の `HttpConnectionPool` と異なり、HTTP/2 はコネクション上で
/// ストリーム ID を単調増加させながら複数リクエストを直列に流せる（`H2cClient` の
/// `next_stream_id` が状態を保持）。プールに返す前に呼び出し側が
/// `H2cClient::is_reusable()`（ストリーム ID 枯渇前）と応答成功を確認する。
/// io_uring の `TcpStream` はワーカースレッドの ring に紐づくため、スレッドローカルで
/// 同一スレッド再利用のみ行う（thread-per-core）。
#[cfg(feature = "http2")]
pub(crate) struct H2cConnectionPool {
    connections: HashMap<String, VecDeque<PooledConnection<crate::http2::H2cClient<TcpStream>>>>,
}

#[cfg(feature = "http2")]
impl H2cConnectionPool {
    pub(crate) fn new() -> Self {
        Self {
            connections: HashMap::new(),
        }
    }

    /// プールから接続を取得（有効な接続がなければ None）
    pub(crate) fn get(&mut self, key: &str) -> Option<crate::http2::H2cClient<TcpStream>> {
        if let Some(queue) = self.connections.get_mut(key) {
            while let Some(entry) = queue.pop_front() {
                if entry.is_valid() {
                    crate::metrics::record_connection_pool_hit(key);
                    return Some(entry.stream);
                }
                // 無効（アイドルタイムアウト超過）な接続は破棄
            }
        }
        crate::metrics::record_connection_pool_miss(key);
        None
    }

    /// 接続をプールに返却（`is_reusable()` を満たす健全な接続のみ返す）
    pub(crate) fn put(
        &mut self,
        key: String,
        client: crate::http2::H2cClient<TcpStream>,
        max_idle: usize,
        idle_timeout_secs: u64,
    ) {
        let metric_key = key.clone();
        let queue = self.connections.entry(key).or_default();

        while queue.len() >= max_idle {
            queue.pop_front();
        }

        queue.push_back(PooledConnection::new(client, idle_timeout_secs));
        crate::metrics::set_connection_pool_size(&metric_key, queue.len());
    }
}

thread_local! {
    pub(crate) static HTTP_POOL: RefCell<HttpConnectionPool> = RefCell::new(HttpConnectionPool::new());
    pub(crate) static HTTPS_POOL: RefCell<HttpsConnectionPool> = RefCell::new(HttpsConnectionPool::new());
}

#[cfg(feature = "http2")]
thread_local! {
    pub(crate) static H2C_POOL: RefCell<H2cConnectionPool> = RefCell::new(H2cConnectionPool::new());
}

// kTLS 有効時のスレッドローカル Splice パイプの checkout/return 型プール（B-16）
// splice(2) によるゼロコピー転送に使用。
//
// 旧実装は `RefCell<Option<SplicePipe>>` の `Ref` を `'static` に transmute して
// 返却しており、呼び出し側が await を跨いで `Ref` を保持したまま同一スレッドの
// 別タスクが再度 `borrow_mut()`（遅延初期化）を実行すると `BorrowMutError` panic、
// さらに同一パイプの並行 splice によるデータ混線のリスクがあった。
// 本実装は L4 パイプツール（F-40）と同じ checkout/return 方式:
// 取得時にプールから所有権ごと取り出し（借用を await 跨ぎで保持しない）、
// Drop 時に FIONREAD で空を確認できた場合のみプールへ返却する。
#[cfg(feature = "ktls")]
thread_local! {
    static SPLICE_PIPE_POOL: RefCell<Vec<ktls_rustls::SplicePipe>> =
        const { RefCell::new(Vec::new()) };
}

/// プールに保持するパイプ本数の上限（スレッドごと）。
/// kTLS splice 経路は 1 リクエストあたり 1 本使用するため、同時 64 リクエスト分をカバーする。
#[cfg(feature = "ktls")]
const SPLICE_PIPE_POOL_MAX: usize = 64;

/// プールから取得した splice パイプの RAII ガード。
///
/// 所有権ベースのため await を跨いで保持しても `RefCell` の借用は残らない。
/// Drop 時、パイプに残データが無い（FIONREAD == 0）場合のみプールへ返却する。
/// 残データがあるパイプを再利用すると次のリクエストへデータが混線するため、
/// それ以外（残データあり・ioctl 失敗・プール満杯）は破棄する（fd クローズ）。
#[cfg(feature = "ktls")]
pub(crate) struct PooledSplicePipe {
    pipe: Option<ktls_rustls::SplicePipe>,
}

#[cfg(feature = "ktls")]
impl std::ops::Deref for PooledSplicePipe {
    type Target = ktls_rustls::SplicePipe;

    fn deref(&self) -> &Self::Target {
        // 不変条件: `pipe` は Drop まで必ず Some（take するのは drop 内のみ）
        self.pipe.as_ref().unwrap()
    }
}

#[cfg(feature = "ktls")]
impl Drop for PooledSplicePipe {
    fn drop(&mut self) {
        let Some(pipe) = self.pipe.take() else {
            return;
        };
        let mut pending: libc::c_int = 0;
        let ret = unsafe { libc::ioctl(pipe.read_fd(), libc::FIONREAD, &mut pending) };
        if ret != 0 || pending != 0 {
            // 残データあり or ioctl 失敗: 破棄（SplicePipe::drop が fd をクローズ）
            return;
        }
        SPLICE_PIPE_POOL.with(|pool| {
            let mut pool = pool.borrow_mut();
            if pool.len() < SPLICE_PIPE_POOL_MAX {
                pool.push(pipe);
            }
            // 満杯なら Drop で破棄
        });
    }
}

/// スレッドローカルプールから splice パイプを取得する（B-16: 所有権ベース）。
///
/// プールが空の場合は新規作成（`pipe2(2)`）にフォールバックする。
/// 作成に失敗した場合は `None` を返す（呼び出し側は通常転送へフォールバック）。
#[cfg(feature = "ktls")]
pub(crate) fn get_splice_pipe() -> Option<PooledSplicePipe> {
    if let Some(pipe) = SPLICE_PIPE_POOL.with(|pool| pool.borrow_mut().pop()) {
        return Some(PooledSplicePipe { pipe: Some(pipe) });
    }
    match ktls_rustls::SplicePipe::new() {
        Ok(pipe) => Some(PooledSplicePipe { pipe: Some(pipe) }),
        Err(e) => {
            ftlog::warn!("Failed to create splice pipe: {}", e);
            None
        }
    }
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

// ====================
// Alt-Svc ヘッダー（HTTP/3 広告、F-94）
// ====================
//
// `server.http3_enabled` 時に HTTP/1.1 / HTTP/2 応答へ `Alt-Svc: h3=":port"; ma=...`
// を付与し、ブラウザ等クライアントが HTTP/3 へアップグレードできるようにする。
// Server ヘッダーと同じく ArcSwap + AtomicBool でホットパスのゼロコピー参照を実現。

/// Alt-Svc ヘッダー値（起動時/リロード時に更新）
pub(crate) static ALT_SVC_VALUE: Lazy<arc_swap::ArcSwap<Vec<u8>>> =
    Lazy::new(|| arc_swap::ArcSwap::from(Arc::new(Vec::new())));

/// Alt-Svc ヘッダー有効フラグ
pub(crate) static ALT_SVC_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Alt-Svc 設定を安全に取得（Guard でリクエスト処理中の無効化を防ぐ）
#[inline]
pub fn get_alt_svc_guard() -> Option<AltSvcGuard> {
    if ALT_SVC_ENABLED.load(std::sync::atomic::Ordering::Acquire) {
        let guard = ALT_SVC_VALUE.load();
        if !guard.is_empty() {
            Some(AltSvcGuard { guard })
        } else {
            None
        }
    } else {
        None
    }
}

/// Alt-Svc 値を保持する Guard
pub struct AltSvcGuard {
    pub(crate) guard: arc_swap::Guard<Arc<Vec<u8>>>,
}

impl AltSvcGuard {
    /// ヘッダータプルとして取得（ゼロコピー）。名前は小文字（HTTP/2/HPACK 向け）。
    #[inline]
    pub fn as_header(&self) -> (&'static [u8], &[u8]) {
        (b"alt-svc", self.guard.as_slice())
    }

    /// 値のスライスとして取得
    #[inline]
    pub fn value(&self) -> &[u8] {
        self.guard.as_slice()
    }
}

/// Alt-Svc 設定を初期化
///
/// * `enabled` - 広告を有効にするか（通常は `http3_enabled` と連動）
/// * `value` - ヘッダー値全文（例: `h3=":8443"; ma=86400`）。空なら無効扱い
pub(crate) fn init_alt_svc(enabled: bool, value: &str) {
    if !value.is_empty() {
        ALT_SVC_VALUE.store(Arc::new(value.as_bytes().to_vec()));
    } else {
        ALT_SVC_VALUE.store(Arc::new(Vec::new()));
    }
    let effective = enabled && !value.is_empty();
    ALT_SVC_ENABLED.store(effective, std::sync::atomic::Ordering::Release);
    info!(
        "Alt-Svc header: {} (value: {:?})",
        if effective { "enabled" } else { "disabled" },
        if effective { value } else { "" }
    );
}

/// HTTP/3 リッスンアドレスから標準的な Alt-Svc 値を構築する（コールドパス）。
///
/// `listen` 例: `"127.0.0.1:8443"` / `"[::]:443"` / `"0.0.0.0:443"`
/// → `h3=":8443"; ma=86400`（ホスト省略のポート広告。RFC 7838 / RFC 9114）
pub fn build_alt_svc_value(listen: &str, ma_secs: u64) -> String {
    let port = parse_listen_port(listen).unwrap_or(443);
    format!("h3=\":{}\"; ma={}", port, ma_secs)
}

/// HTTP/1.1 応答バッファへ `Alt-Svc: ...\r\n` を追記（有効時のみ、値はゼロコピー）。
#[inline]
pub fn append_alt_svc_header_line(buf: &mut Vec<u8>) {
    if let Some(g) = get_alt_svc_guard() {
        buf.extend_from_slice(b"Alt-Svc: ");
        buf.extend_from_slice(g.value());
        buf.extend_from_slice(b"\r\n");
    }
}

/// `host:port` / `[ipv6]:port` からポートを抽出（失敗時 None）
fn parse_listen_port(listen: &str) -> Option<u16> {
    let s = listen.trim();
    if let Some(bracket_end) = s.rfind(']') {
        // [ipv6]:port
        let rest = s.get(bracket_end + 1..)?;
        let port_str = rest.strip_prefix(':')?;
        return port_str.parse().ok();
    }
    // host:port
    let port_str = s.rsplit_once(':')?.1;
    port_str.parse().ok()
}

#[cfg(test)]
mod alt_svc_tests {
    use super::*;

    #[test]
    fn build_alt_svc_value_ipv4() {
        assert_eq!(
            build_alt_svc_value("127.0.0.1:8443", 86400),
            "h3=\":8443\"; ma=86400"
        );
    }

    #[test]
    fn build_alt_svc_value_default_port() {
        assert_eq!(
            build_alt_svc_value("0.0.0.0:443", 3600),
            "h3=\":443\"; ma=3600"
        );
    }

    #[test]
    fn build_alt_svc_value_ipv6() {
        assert_eq!(
            build_alt_svc_value("[::1]:9443", 86400),
            "h3=\":9443\"; ma=86400"
        );
    }

    #[test]
    fn parse_listen_port_invalid() {
        assert!(parse_listen_port("no-port").is_none());
        assert!(parse_listen_port("").is_none());
    }

    #[test]
    fn init_and_get_alt_svc_guard() {
        init_alt_svc(true, "h3=\":8443\"; ma=86400");
        let g = get_alt_svc_guard().expect("guard");
        let (name, value) = g.as_header();
        assert_eq!(name, b"alt-svc");
        assert_eq!(value, b"h3=\":8443\"; ma=86400");
        init_alt_svc(false, "h3=\":8443\"; ma=86400");
        assert!(get_alt_svc_guard().is_none());
        // テスト間汚染防止
        init_alt_svc(false, "");
    }
}

// ====================
// テスト
// ====================

#[cfg(test)]
mod tests {
    // B-16: splice パイプの checkout/return プールの回帰テスト。
    // 各テストは独立スレッドで実行されるため thread_local プールは常に空から始まる。
    #[cfg(feature = "ktls")]
    mod splice_pipe_pool {
        use super::super::*;

        fn pool_len() -> usize {
            SPLICE_PIPE_POOL.with(|p| p.borrow().len())
        }

        /// 複数のガードを同時に保持できること（旧実装では 2 回目の取得で
        /// `RefCell` 二重借用 panic した経路の回帰テスト）。
        #[test]
        fn test_concurrent_guards_do_not_panic() {
            let a = get_splice_pipe().expect("pipe a");
            let b = get_splice_pipe().expect("pipe b");
            // 別々のパイプが払い出される（同一パイプの並行使用によるデータ混線がない）
            assert_ne!(a.read_fd(), b.read_fd());
            assert_ne!(a.write_fd(), b.write_fd());
            drop(a);
            drop(b);
            assert_eq!(pool_len(), 2);
        }

        /// 空のパイプは Drop でプールへ返却され、次の取得で再利用されること。
        #[test]
        fn test_reuses_clean_pipe() {
            let pipe = get_splice_pipe().expect("pipe");
            let fd = pipe.read_fd();
            drop(pipe);
            assert_eq!(pool_len(), 1);
            let pipe2 = get_splice_pipe().expect("pipe2");
            assert_eq!(pipe2.read_fd(), fd);
        }

        /// 残データのあるパイプは返却されず破棄されること（データ混線防止）。
        #[test]
        fn test_discards_dirty_pipe() {
            let pipe = get_splice_pipe().expect("pipe");
            let buf = [0u8; 8];
            let n = unsafe {
                libc::write(
                    pipe.write_fd(),
                    buf.as_ptr() as *const libc::c_void,
                    buf.len(),
                )
            };
            assert_eq!(n, 8);
            drop(pipe);
            assert_eq!(pool_len(), 0);
        }

        /// プールが上限を超えて肥大化しないこと。
        #[test]
        fn test_respects_max() {
            let guards: Vec<_> = (0..SPLICE_PIPE_POOL_MAX + 8)
                .map(|_| get_splice_pipe().expect("pipe"))
                .collect();
            drop(guards);
            assert_eq!(pool_len(), SPLICE_PIPE_POOL_MAX);
        }
    }

    // B-44: バックエンドコネクションプールが max_idle まで保持し、それ以上は
    // 最古のものから破棄することの回帰テスト（BACKEND_POOL_MAX_IDLE_PER_HOST = 256）。
    mod http_connection_pool {
        use super::super::*;

        /// テスト用のダミー TcpStream を作る（socketpair の片端を io_uring TcpStream として
        /// ラップする）。put/get の保持数検証のみが目的でデータの送受信は行わない。
        fn dummy_tcp_stream() -> TcpStream {
            let mut fds = [0 as std::os::unix::io::RawFd; 2];
            let ret =
                unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) };
            assert_eq!(ret, 0, "socketpair failed");
            // 片方はテスト側で即座にクローズし、もう片方を TcpStream にラップする。
            unsafe { libc::close(fds[1]) };
            unsafe { TcpStream::from_raw_fd(fds[0]) }
        }

        #[test]
        fn test_put_respects_max_idle_256() {
            let mut pool = HttpConnectionPool::new();
            let key = "example.test:80";
            for _ in 0..(BACKEND_POOL_MAX_IDLE_PER_HOST + 8) {
                pool.put(
                    key.to_string(),
                    dummy_tcp_stream(),
                    BACKEND_POOL_MAX_IDLE_PER_HOST,
                    BACKEND_POOL_IDLE_TIMEOUT_SECS,
                );
            }
            let len = pool.connections.get(key).map(|q| q.len()).unwrap_or(0);
            assert_eq!(
                len, BACKEND_POOL_MAX_IDLE_PER_HOST,
                "pool should retain exactly max_idle (256) connections, discarding oldest"
            );
        }

        #[test]
        fn test_put_below_max_idle_retains_all() {
            let mut pool = HttpConnectionPool::new();
            let key = "example.test:80";
            let n = 10;
            for _ in 0..n {
                pool.put(
                    key.to_string(),
                    dummy_tcp_stream(),
                    BACKEND_POOL_MAX_IDLE_PER_HOST,
                    BACKEND_POOL_IDLE_TIMEOUT_SECS,
                );
            }
            let len = pool.connections.get(key).map(|q| q.len()).unwrap_or(0);
            assert_eq!(len, n, "below max_idle, all connections should be retained");
        }
    }
}
