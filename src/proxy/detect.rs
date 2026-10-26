//! プロトコル検出（HTTP/1.1・h2c）
//!
//! F-233 で `src/proxy.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// プロトコル検出機能（H2C対応）
// ====================

/// プロトコルタイプ
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolType {
    H2C,     // HTTP/2 Cleartext (Prior Knowledge)
    TLS,     // TLS (HTTPS)
    Http11,  // HTTP/1.1
    Unknown, // 不明
    /// 1 バイトも届かずに閉じられた・アイドルタイムアウト・graceful shutdown（TCP のヘルスチェック等。B-122）
    Closed,
}

/// プロトコル判別で読み取った初期データを保持し、
/// 以降の読み取りでそのデータを優先して返すラッパーストリーム。
/// これにより、TLSハンドシェイク等の後続処理でデータ欠落を防ぐ。
pub struct BufferedStream<S> {
    inner: S,
    buffer: Option<Vec<u8>>,
    pos: usize,
}

impl<S> BufferedStream<S> {
    pub fn new(inner: S, initial_data: Vec<u8>) -> Self {
        let buffer = if initial_data.is_empty() {
            None
        } else {
            Some(initial_data)
        };
        BufferedStream {
            inner,
            buffer,
            pos: 0,
        }
    }
}

impl<S: AsRawFd> AsRawFd for BufferedStream<S> {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl<S> crate::runtime::io::BufferedReadState for BufferedStream<S> {
    /// プロトコル検出時の先読みデータが未消費なら `true`（消費し切ると `buffer` は `None`）。
    /// F-116: HTTP/2 多重化メインループの可読待機前チェックに使う。
    #[inline]
    fn has_buffered_read_data(&self) -> bool {
        self.buffer.is_some()
    }
}

impl<S: AsyncReadRent + Unpin> AsyncReadRent for BufferedStream<S> {
    async fn read<T: IoBufMut>(&mut self, mut buf: T) -> crate::runtime::io::BufResult<usize, T> {
        if let Some(ref b) = self.buffer {
            let remaining = b.len() - self.pos;
            let to_copy = std::cmp::min(remaining, buf.bytes_total());

            unsafe {
                let slice = std::slice::from_raw_parts_mut(buf.write_ptr(), buf.bytes_total());
                slice[..to_copy].copy_from_slice(&b[self.pos..self.pos + to_copy]);
                buf.set_init(to_copy);
            }

            self.pos += to_copy;
            if self.pos >= b.len() {
                self.buffer = None;
            }

            return (Ok(to_copy), buf);
        }

        self.inner.read(buf).await
    }

    async fn readv<T: IoVecBufMut>(&mut self, buf: T) -> crate::runtime::io::BufResult<usize, T> {
        // 現在の実装では readv は使用しないため、バッファがある場合は未サポート
        if self.buffer.is_some() {
            return (
                Err(io::Error::other(
                    "readv not supported for BufferedStream with data",
                )),
                buf,
            );
        }
        self.inner.readv(buf).await
    }
}

impl<S: crate::runtime::io::AsyncWriteRent + Unpin> crate::runtime::io::AsyncWriteRent
    for BufferedStream<S>
{
    async fn write<T: IoBuf>(&mut self, buf: T) -> crate::runtime::io::BufResult<usize, T> {
        self.inner.write(buf).await
    }

    async fn writev<T: IoVecBuf>(&mut self, buf: T) -> crate::runtime::io::BufResult<usize, T> {
        self.inner.writev(buf).await
    }

    async fn flush(&mut self) -> io::Result<()> {
        self.inner.flush().await
    }

    async fn shutdown(&mut self) -> io::Result<()> {
        self.inner.shutdown().await
    }
}

/// プロトコル検出とバッファ管理
///
/// 最初の数バイトを読み込んでプロトコルを判別します。
/// 読み込んだデータはバッファに保持し、後続の処理で再利用します。
///
/// 検出順序:
/// 1. HTTP/2プリフェース（24バイト固定）
/// 2. TLS ClientHello（最初の5バイト）
/// 3. HTTP/1.1リクエスト（最初の3バイトでメソッド検出）
///
/// 待ち方（B-122）:
/// - 最初のバイトまでは、キープアライブで次の要求を待つのと同じアイドル待ち（`IDLE_TIMEOUT`、graceful shutdown で
///   即座に満了）。ブラウザの事前接続や接続プールは、接続してから要求を送るまで間を空けることがある
///   （以前は 200ms で打ち切って Unknown とし、HTTP/1.1 のクライアントへ HTTP/2 の GOAWAY を返していた）。
/// - 判別に足りない断片（5 バイト未満・HTTP/2 プリフェースの途中）の続きは短い上限（1 秒）で待つ。覗き直しの間隔は
///   1ms から倍々に 50ms まで延ばす（止まった断片 1 本あたりの起床は約 25 回。以前は 200ms を 1ms 間隔で 200 回。
///   200ms では負荷の高いホストで 50ms 遅れの続きが間に合わないことがあった。v0.8.0）。
/// - 1 バイトも届かずに閉じられた・タイムアウトした接続は `Closed`、判別できないバイト列は `Unknown`。
pub async fn detect_protocol_with_buffer(stream: &mut TcpStream) -> (ProtocolType, Vec<u8>) {
    // ソケットバッファを MSG_PEEK で覗き見て（消費せず）プロトコルを判別する。
    //
    // 以前は io_uring RECV でバイトを消費し initial_data として持ち回していたが、
    // タイムアウトキャンセルや所有権の絡みでバイトが欠落/破損し、後続の TLS ハンドシェイクが
    // `received corrupt message / InvalidContentType` で失敗する不具合があった
    // （h2c 検出が有効な全接続で発生しうる）。MSG_PEEK ならバイトはソケットに残るため、
    // 判別後に TLS/H2C/HTTP1.1 各ハンドラがそのまま読み直せる（initial_data は常に空）。
    const PREFACE: &[u8; 24] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
    const PARTIAL_TIMEOUT: Duration = Duration::from_secs(1);
    const PARTIAL_POLL_MAX: Duration = Duration::from_millis(50);
    let fd = stream.as_raw_fd();
    let mut peeked = [0u8; 24];
    let mut n = 0usize;
    // 断片を最初に覗いた時刻（続きを待つ上限の起点）
    let mut partial_since: Option<std::time::Instant> = None;
    // 断片の続きを覗き直す間隔（倍々に延ばす）
    let mut partial_poll = Duration::from_millis(1);

    loop {
        // データ到着を待つ（MSG_PEEK は消費しないのでタイムアウトキャンセルしても安全）。
        match partial_since {
            None => match timeout_idle(crate::pool::IDLE_TIMEOUT, true, stream.readable()).await {
                Ok(Ok(())) => {}
                _ => break,
            },
            Some(since) => {
                if since.elapsed() >= PARTIAL_TIMEOUT {
                    break;
                }
                // 断片はソケットに残ったまま（覗いただけ）なので readable は即座に成立する。
                // 続きを待つ間は readiness ではなく短い sleep で覗き直す（断片は稀な経路）。
                crate::runtime::time::sleep(partial_poll).await;
                partial_poll = (partial_poll * 2).min(PARTIAL_POLL_MAX);
            }
        }

        // 消費せずに覗き見る（毎回ソケットバッファの先頭から最大 24 バイト）。
        // Windows には `MSG_DONTWAIT` が無いが、ソケット自体が非ブロッキングのため
        // `MSG_PEEK` 単独で同等に動作する。
        #[cfg(unix)]
        let ret = unsafe {
            libc::recv(
                fd,
                peeked.as_mut_ptr() as *mut libc::c_void,
                peeked.len(),
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        #[cfg(windows)]
        let ret = unsafe {
            windows_sys::Win32::Networking::WinSock::recv(
                crate::runtime::handle::win::to_socket(fd),
                peeked.as_mut_ptr(),
                peeked.len() as i32,
                windows_sys::Win32::Networking::WinSock::MSG_PEEK,
            )
        };
        #[cfg(windows)]
        let last_err = || {
            io::Error::from_raw_os_error(unsafe {
                windows_sys::Win32::Networking::WinSock::WSAGetLastError()
            })
        };
        #[cfg(unix)]
        let last_err = io::Error::last_os_error;
        if ret < 0 {
            // 偽の readable 通知（EAGAIN）なら再試行、その他はフォールバック。
            if last_err().kind() == io::ErrorKind::WouldBlock {
                continue;
            }
            break;
        }
        if ret == 0 {
            break; // 接続終了
        }
        n = ret as usize;
        // TLS/HTTP1.1 は 5 バイト、H2C プリフェースは 24 バイトで判別可能。
        // プリフェースの途中（"PRI * ..." の 24 バイト未満）なら続きを待つ。
        if n >= 24 || (n >= 5 && !PREFACE.starts_with(&peeked[..n])) {
            break;
        }
        partial_since.get_or_insert_with(std::time::Instant::now);
    }

    if n == 0 {
        return (ProtocolType::Closed, Vec::new());
    }

    let data = &peeked[..n];

    // HTTP/2 プリフェース検出（24バイト固定）。MSG_PEEK なので initial_data は空で返す。
    if n >= 24 && data == PREFACE {
        debug!("[Protocol Detection] H2C (HTTP/2 Cleartext) detected");
        return (ProtocolType::H2C, Vec::new());
    }

    if n >= 5 {
        if data[0] == 0x16 && data[1] == 0x03 && (data[2] >= 0x01 && data[2] <= 0x03) {
            debug!("[Protocol Detection] TLS detected");
            return (ProtocolType::TLS, Vec::new());
        }

        let methods: [&[u8]; 9] = [
            b"GET ", b"POST", b"PUT ", b"DELE", b"HEAD", b"OPTI", b"CONN", b"TRAC", b"PATC",
        ];
        for method in &methods {
            if data.starts_with(method) {
                debug!("[Protocol Detection] HTTP/1.1 detected");
                return (ProtocolType::Http11, Vec::new());
            }
        }
    }

    debug!(
        "[Protocol Detection] Unknown protocol ({} bytes): hex={:02x?}",
        n, data
    );

    (ProtocolType::Unknown, Vec::new())
}

/// h2c 専用リスナーで HTTP/2 プリフェース以外を受けた接続を閉じる（エラー経路のみ）。
///
/// RFC 9113 §3.4: 不正なプリフェースは PROTOCOL_ERROR のコネクションエラー。固定の
/// GOAWAY(last_stream_id=0, PROTOCOL_ERROR) を送り、送信側を閉じてから相手の close まで
/// （時間・量の上限付きで）読み捨てる。未読データを残して close すると TCP は FIN ではなく
/// RST を送り、相手が GOAWAY を読む前に捨てられ得る（h2spec 3.5/2 で検出）。
#[cfg(feature = "http2")]
pub async fn reject_h2c_invalid_preface(mut stream: TcpStream) {
    const LINGER: Duration = Duration::from_millis(500);
    const MAX_DRAIN: usize = 64 * 1024;
    // length=8, type=GOAWAY(0x7), flags=0, stream=0 / last_stream_id=0, error=PROTOCOL_ERROR(0x1)
    const GOAWAY_PROTOCOL_ERROR: [u8; 17] = [0, 0, 8, 0x7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x1];
    let _ = timeout(LINGER, stream.write_all(GOAWAY_PROTOCOL_ERROR.to_vec())).await;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = timeout(LINGER, async {
        let mut drained = 0usize;
        let mut buf = vec![0u8; 4096];
        while drained < MAX_DRAIN {
            let (res, b) = stream.read(buf).await;
            buf = b;
            match res {
                Ok(n) if n > 0 => drained += n,
                _ => break,
            }
        }
    })
    .await;
}

/// 平文リスナー（`[server].listen_cleartext`）で受けた HTTP/1.1 接続を処理する（F-200）。
///
/// プロトコル検出で読んだ先頭バイト列（`initial_data`）を平文ストリームに引き継ぎ、
/// TLS 接続と同じ `handle_requests` で処理する。
#[cfg(feature = "http2")]
pub async fn handle_plain_http1_connection(
    stream: TcpStream,
    initial_data: Vec<u8>,
    peer_addr: SocketAddr,
) {
    #[cfg(veil_ktls)]
    let plain = crate::ktls_rustls::accept_plain(stream, Some(initial_data)).await;
    #[cfg(not(veil_ktls))]
    let plain = simple_tls::accept_plain(stream, Some(initial_data)).await;
    match plain {
        Ok(plain_stream) => {
            let client_ip = IpStr::new(peer_addr.ip());
            handle_requests(plain_stream, client_ip.as_str(), peer_addr).await;
        }
        Err(e) => warn!("Failed to create plain stream: {}", e),
    }
}

/// 平文リスナーで `allow_http = false` のとき HTTP/1.1 を 426 で断る（F-200）。
///
/// 未読データを残して close すると RST になるため、送信側を閉じてから上限付きで読み捨てる（B-102）。
#[cfg(feature = "http2")]
pub async fn reject_plain_http1(mut stream: TcpStream) {
    const LINGER: Duration = Duration::from_millis(500);
    const MAX_DRAIN: usize = 64 * 1024;
    const RESP: &[u8] = b"HTTP/1.1 426 Upgrade Required\r\nUpgrade: TLS/1.2, HTTP/1.1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let _ = timeout(LINGER, stream.write_all(RESP.to_vec())).await;
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = timeout(LINGER, async {
        let mut drained = 0usize;
        let mut buf = vec![0u8; 4096];
        while drained < MAX_DRAIN {
            let (res, b) = stream.read(buf).await;
            buf = b;
            match res {
                Ok(n) if n > 0 => drained += n,
                _ => break,
            }
        }
    })
    .await;
}

/// H2Cサーバー接続処理
///
/// TLSなしでHTTP/2コネクションを確立し、リクエストを処理します。
#[cfg(feature = "http2")]
pub async fn handle_h2c_connection(
    stream: TcpStream,
    client_ip: &str,
    initial_data: Vec<u8>,
    redirect_https: Option<u16>,
) {
    use http2::Http2Connection;

    // HTTP/2設定を取得
    let config = CURRENT_CONFIG.load();
    let settings = config.http2_config.to_http2_settings();

    // Http2ConnectionはTcpStreamでも動作可能
    // 既に読み込んだデータ（プリフェース）を初期バッファとして渡す
    // これにより、不要な再読み込みを回避できる
    // FreeBSD: 平文なので、静的コンテンツキャッシュ（共有メモリ）の DATA を sendfile(2) で送ってよい
    #[cfg(all(target_os = "freebsd", feature = "cache"))]
    let sock_fd = stream.as_raw_fd();
    let mut conn = Http2Connection::new_with_initial_buffer(stream, settings, initial_data);
    #[cfg(all(target_os = "freebsd", feature = "cache"))]
    conn.enable_zero_copy(sock_fd);

    // ハンドシェイク（プリフェース確認 + SETTINGS 交換）
    // expect_preface()は初期バッファからプリフェースを読み取る
    if let Err(e) = conn.handshake().await {
        warn!("[H2C] Handshake error: {}", e);
        return;
    }

    debug!("[H2C] Connection established from {}", client_ip);

    // アクティブ接続メトリクスの自動管理（Dropで自動デクリメント）
    let mut connection_metric = ActiveConnectionMetric::new(true);

    // 既存のHTTP/2リクエスト処理を使用
    let result = handle_http2_requests(
        &mut conn,
        client_ip,
        &mut connection_metric,
        redirect_https,
        false,
    )
    .await;

    if let Err(e) = result {
        warn!("[H2C] Connection error: {}", e);
    }

    debug!("[H2C] Connection closed from {}", client_ip);
}

// kTLS 有効時の接続処理（rustls + ktls2）
#[cfg(veil_ktls)]
pub async fn handle_connection(stream: TcpStream, acceptor: RustlsAcceptor, peer_addr: SocketAddr) {
    // F-200: TLS のリスナー（`[server].listen`）は常に TLS だけを受ける（平文は `listen_cleartext`）。
    let initial_buffer = None;

    // TLSハンドシェイクにタイムアウトを設定
    // rustls でハンドシェイク後、ktls2 で kTLS を有効化
    let tls_result = timeout(CONNECT_TIMEOUT, acceptor.accept(stream, initial_buffer)).await;

    let tls_stream = match tls_result {
        Ok(Ok(tls)) => tls,
        Ok(Err(e)) => {
            warn!("TLS handshake error: {}", e);
            return;
        }
        Err(_) => {
            warn!("TLS handshake timeout");
            return;
        }
    };

    // クライアントIPアドレスをスタックバッファへ変換（F-41: 接続ごとのヒープ確保排除）
    let client_ip = IpStr::new(peer_addr.ip());

    // ALPN で h2 がネゴシエートされたら HTTP/2 ハンドラーを使用する。ALPN は起動時の
    // `http2_enabled` で決まり、リロードでは変わらないので、振り分けもネゴシエート結果だけで決める
    // （リロード後の設定を見ると、h2 を出したまま HTTP/1.1 で処理してしまう）。
    #[cfg(feature = "http2")]
    if tls_stream.is_http2() {
        handle_http2_connection(tls_stream, client_ip.as_str()).await;
        return;
    }

    // HTTP/1.1 ハンドラー
    handle_requests(tls_stream, client_ip.as_str(), peer_addr).await;
}

// kTLS 無効時の接続処理（rustls のみ）
#[cfg(not(veil_ktls))]
pub async fn handle_connection(
    stream: TcpStream,
    acceptor: simple_tls::SimpleTlsAcceptor,
    peer_addr: SocketAddr,
) {
    // F-200: TLS のリスナー（`[server].listen`）は常に TLS だけを受ける（平文は `listen_cleartext`）。
    let initial_buffer = None;

    // TLSハンドシェイクにタイムアウトを設定
    let tls_result = timeout(CONNECT_TIMEOUT, acceptor.accept(stream, initial_buffer)).await;

    let tls_stream = match tls_result {
        Ok(Ok(tls)) => tls,
        Ok(Err(e)) => {
            warn!("TLS handshake error: {}", e);
            return;
        }
        Err(_) => {
            warn!("TLS handshake timeout");
            return;
        }
    };

    // クライアントIPアドレスをスタックバッファへ変換（F-41: 接続ごとのヒープ確保排除）
    let client_ip = IpStr::new(peer_addr.ip());

    // ALPN で h2 がネゴシエートされたら HTTP/2 ハンドラーを使用する。ALPN は起動時の
    // `http2_enabled` で決まり、リロードでは変わらないので、振り分けもネゴシエート結果だけで決める
    // （リロード後の設定を見ると、h2 を出したまま HTTP/1.1 で処理してしまう）。
    #[cfg(feature = "http2")]
    if tls_stream.is_http2() {
        handle_http2_connection(tls_stream, client_ip.as_str()).await;
        return;
    }

    // HTTP/1.1 ハンドラー
    handle_requests(tls_stream, client_ip.as_str(), peer_addr).await;
}
