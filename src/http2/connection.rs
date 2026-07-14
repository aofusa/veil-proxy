//! # HTTP/2 コネクション管理 (RFC 7540)
//!
//! HTTP/2 コネクションの確立、フレーム処理、ストリーム管理を行います。
//! monoio 非同期ランタイムと統合して動作します。

use std::io;

use crate::runtime::io::{AsyncReadRent, AsyncWriteRentExt};

use crate::http2::error::{Http2Error, Http2ErrorCode, Http2Result};
use crate::http2::frame::{Frame, FrameDecoder, FrameEncoder, FrameHeader};
use crate::http2::hpack::{HpackDecoder, HpackEncoder};
use crate::http2::settings::{defaults, Http2Settings};
use crate::http2::stream::{Stream, StreamManager, StreamState};

/// HTTP/2 コネクションプリフェース
pub const CONNECTION_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// HTTP/2 コネクション (サーバー側)
pub struct Http2Connection<S> {
    /// TLS ストリーム
    stream: S,
    /// ローカル設定 (サーバー)
    local_settings: Http2Settings,
    /// リモート設定 (クライアント)
    remote_settings: Http2Settings,
    /// ストリームマネージャー
    streams: StreamManager,
    /// HPACK デコーダ
    hpack_decoder: HpackDecoder,
    /// HPACK エンコーダ
    hpack_encoder: HpackEncoder,
    /// フレームエンコーダ
    frame_encoder: FrameEncoder,
    /// フレームデコーダ
    frame_decoder: FrameDecoder,
    /// コネクションレベル送信ウィンドウ
    conn_send_window: i32,
    /// コネクションレベル受信ウィンドウ
    conn_recv_window: i32,
    /// GOAWAY 送信済みフラグ
    goaway_sent: bool,
    /// GOAWAY 受信済みフラグ
    goaway_received: bool,
    /// GOAWAY で受信した last_stream_id (RFC 7540 Section 6.8)
    goaway_last_stream_id: Option<u32>,
    /// SETTINGS ACK 待ち
    settings_ack_pending: bool,
    /// 読み込みバッファ
    read_buf: Vec<u8>,
    /// バッファ内の有効データ開始位置
    buf_start: usize,
    /// バッファ内の有効データ終了位置
    buf_end: usize,

    // ====================
    // DoS 対策用状態
    // ====================
    /// RST_STREAM カウンター (Rapid Reset 対策)
    rst_stream_count: u32,
    /// RST_STREAM ウィンドウ開始時刻
    rst_stream_window_start: std::time::Instant,

    /// 制御フレームカウンター (Control Frame Flooding 対策)
    control_frame_count: u32,
    /// 制御フレームウィンドウ開始時刻
    control_frame_window_start: std::time::Instant,

    /// 現在のストリームの CONTINUATION カウンター
    continuation_count: u32,

    /// 送信フレーム連結バッファ（送信ホットパス最適化）
    ///
    /// 1 レスポンス分の HEADERS/DATA/トレイラーを連結し、io_uring への書き込みを
    /// 1 回にまとめる再利用バッファ。呼び出し境界では常に空（`flush_write_buf` 済み）。
    /// 接続をまたいでスレッドローカルプールで再利用する（F-73 続き）。
    write_buf: Vec<u8>,
}

// ====================
// コネクション読み込みバッファのスレッドローカル再利用プール（F-34）
// ====================
// HTTP/2 コネクションは 64KB の読み込みバッファを 1 本確保する。同時接続数が多いと
// 接続ごとの malloc/free が増えるため、スレッドローカルなフリーリストで再利用して
// 確保・解放コストを排除する（thread-per-core 設計のためロック不要）。

const H2_READ_BUF_SIZE: usize = 65536;
/// プールに保持する最大本数（過剰なメモリ保持を防ぐ）
const H2_READ_BUF_POOL_MAX: usize = 256;
/// これを超える肥大化バッファはプールに戻さず解放する（1MB）
const H2_READ_BUF_RETAIN_MAX: usize = 1 << 20;

/// 送信連結バッファの途中フラッシュ閾値（128KB）。
///
/// 大きなボディのストリーミング時に連結バッファが無制限に肥大化しないよう、
/// この閾値を超えたら 1 回書き込んでバッファを空にする。小〜中サイズのレスポンスは
/// 閾値未満のため HEADERS + 全 DATA が 1 回の書き込みにまとまる。
const WRITE_BUF_FLUSH_THRESHOLD: usize = 128 * 1024;

thread_local! {
    static H2_READ_BUF_POOL: std::cell::RefCell<Vec<Vec<u8>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// プールから読み込みバッファを取得する（無ければ新規確保）。`len` は `H2_READ_BUF_SIZE`。
fn acquire_h2_read_buf() -> Vec<u8> {
    let mut buf = H2_READ_BUF_POOL
        .with(|p| p.borrow_mut().pop())
        .unwrap_or_default();
    buf.clear();
    buf.resize(H2_READ_BUF_SIZE, 0);
    buf
}

/// 読み込みバッファをプールへ返却して次の接続で再利用する。空/肥大バッファは戻さない。
fn release_h2_read_buf(mut buf: Vec<u8>) {
    let cap = buf.capacity();
    if cap == 0 || cap > H2_READ_BUF_RETAIN_MAX {
        return;
    }
    buf.clear();
    H2_READ_BUF_POOL.with(|p| {
        let mut pool = p.borrow_mut();
        if pool.len() < H2_READ_BUF_POOL_MAX {
            pool.push(buf);
        }
    });
}

// ====================
// 送信連結バッファのスレッドローカル再利用プール（F-73 続き）
// ====================
// 送信ホットパスの HEADERS/DATA/トレイラー連結バッファを接続をまたいで再利用し、
// 接続ごとの確保・解放コストを排除する（thread-per-core 設計のためロック不要）。

thread_local! {
    static H2_WRITE_BUF_POOL: std::cell::RefCell<Vec<Vec<u8>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// プールから送信連結バッファを取得する（無ければ新規確保・空）。
fn acquire_h2_write_buf() -> Vec<u8> {
    let mut buf = H2_WRITE_BUF_POOL
        .with(|p| p.borrow_mut().pop())
        .unwrap_or_default();
    buf.clear();
    buf
}

/// 送信連結バッファをプールへ返却する。空/肥大バッファは戻さない。
fn release_h2_write_buf(mut buf: Vec<u8>) {
    let cap = buf.capacity();
    if cap == 0 || cap > H2_READ_BUF_RETAIN_MAX {
        return;
    }
    buf.clear();
    H2_WRITE_BUF_POOL.with(|p| {
        let mut pool = p.borrow_mut();
        if pool.len() < H2_READ_BUF_POOL_MAX {
            pool.push(buf);
        }
    });
}

impl<S> Drop for Http2Connection<S> {
    fn drop(&mut self) {
        // 接続終了時に読み込みバッファをプールへ返却（再利用）。fill_read_buf の最中に
        // drop された場合は read_buf が空（take 済み）のため pool には戻らない。
        release_h2_read_buf(std::mem::take(&mut self.read_buf));
        // 送信連結バッファも同様にプールへ返却する。
        release_h2_write_buf(std::mem::take(&mut self.write_buf));
    }
}

impl<S> Http2Connection<S>
where
    S: AsyncReadRent + AsyncWriteRentExt + Unpin,
{
    /// 新しいコネクションを作成
    pub fn new(stream: S, settings: Http2Settings) -> Self {
        Self::new_with_initial_buffer(stream, settings, Vec::new())
    }

    /// 初期バッファデータ付きでコネクションを作成
    ///
    /// プロトコル検出で既に読み込んだデータがある場合に使用します。
    /// これにより、不要な再読み込みを回避できます。
    pub fn new_with_initial_buffer(
        stream: S,
        settings: Http2Settings,
        initial_data: Vec<u8>,
    ) -> Self {
        let hpack_decoder = HpackDecoder::new(settings.header_table_size as usize);
        let hpack_encoder = HpackEncoder::new(settings.header_table_size as usize);
        let frame_encoder = FrameEncoder::new(settings.max_frame_size);
        let frame_decoder = FrameDecoder::new(settings.max_frame_size);
        let streams = StreamManager::new(
            settings.max_concurrent_streams,
            settings.initial_window_size as i32,
        );

        // コネクションウィンドウサイズを設定から取得
        let conn_window = settings.connection_window_size as i32;

        // DoS 対策用のタイムスタンプを初期化
        let now = std::time::Instant::now();

        // 初期バッファを準備（スレッドローカルプールから再利用、F-34）
        let mut read_buf = acquire_h2_read_buf();
        let buf_end = if !initial_data.is_empty() {
            let len = initial_data.len().min(65536);
            read_buf[..len].copy_from_slice(&initial_data[..len]);
            len
        } else {
            0
        };

        Self {
            stream,
            local_settings: settings,
            remote_settings: Http2Settings::default(),
            streams,
            hpack_decoder,
            hpack_encoder,
            frame_encoder,
            frame_decoder,
            conn_send_window: 65535, // RFC 7540 initial window size
            conn_recv_window: conn_window,
            goaway_sent: false,
            goaway_received: false,
            goaway_last_stream_id: None,
            settings_ack_pending: false,
            read_buf,
            buf_start: 0,
            buf_end,
            // DoS 対策
            rst_stream_count: 0,
            rst_stream_window_start: now,
            control_frame_count: 0,
            control_frame_window_start: now,
            continuation_count: 0,
            write_buf: acquire_h2_write_buf(),
        }
    }

    /// HTTP/2 ハンドシェイクを実行
    ///
    /// 1. クライアントプリフェースを受信
    /// 2. サーバー SETTINGS を送信
    /// 3. コネクションウィンドウを拡張 (必要な場合)
    /// 4. クライアント SETTINGS を受信して ACK (run() ループで処理)
    pub async fn handshake(&mut self) -> Http2Result<()> {
        // 1. クライアントプリフェースを受信
        self.expect_preface().await?;

        // 2. サーバー SETTINGS を送信
        self.send_settings().await?;

        // 3. コネクションウィンドウを拡張
        // RFC 7540: デフォルトの 65535 から設定値まで拡張
        let target_window = self.local_settings.connection_window_size as i32;
        let default_window = defaults::CONNECTION_WINDOW_SIZE as i32;
        if target_window > default_window {
            let increment = (target_window - default_window) as u32;
            let frame = self.frame_encoder.encode_window_update(0, increment);
            self.write_all(frame).await?;
        }

        Ok(())
    }

    /// クライアントプリフェースを確認
    async fn expect_preface(&mut self) -> Http2Result<()> {
        let preface_len = CONNECTION_PREFACE.len();

        // プリフェースを読み込む（初期バッファに既にある場合は再読み込み不要）
        while self.buf_end - self.buf_start < preface_len {
            self.fill_read_buf().await?;
        }

        // プリフェースを確認
        let received = &self.read_buf[self.buf_start..self.buf_start + preface_len];
        if received != CONNECTION_PREFACE {
            ftlog::error!("Invalid preface received: {:?}", received);
            return Err(Http2Error::InvalidPreface);
        }

        self.buf_start += preface_len;
        Ok(())
    }

    /// SETTINGS フレームを送信
    async fn send_settings(&mut self) -> Http2Result<()> {
        let settings_payload = self.local_settings.encode();
        let settings: Vec<(u16, u32)> = settings_payload
            .chunks(6)
            .map(|c| {
                let id = u16::from_be_bytes([c[0], c[1]]);
                let val = u32::from_be_bytes([c[2], c[3], c[4], c[5]]);
                (id, val)
            })
            .collect();

        let frame = self.frame_encoder.encode_settings(&settings, false);
        self.write_all(frame).await?;
        self.settings_ack_pending = true;

        Ok(())
    }

    /// フレームを読み込み（外部からアクセス可能）
    ///
    /// HTTP/2 フレームを1つ読み込んでデコードします。
    /// コネクションがクローズされた場合は ConnectionClosed エラーを返します。
    ///
    /// F-116: 同期パース（[`try_read_frame_buffered`](Self::try_read_frame_buffered)）と
    /// 1 回読み込み（[`fill_read_buf`](Self::fill_read_buf)）の合成として再構成した。
    /// ハンドシェイク等の既存呼び出しの挙動は従来と同一。
    pub async fn read_frame(&mut self) -> Http2Result<Frame> {
        loop {
            if let Some(frame) = self.try_read_frame_buffered()? {
                return Ok(frame);
            }
            self.fill_read_buf().await?;
        }
    }

    /// 読み込みバッファ内のデータから完全フレームを 1 つ同期パースする（await しない）。
    ///
    /// `read_buf[buf_start..buf_end]` にフレームヘッダー（9 バイト）+ ペイロード全量が
    /// 揃っていればデコードして `Ok(Some(frame))` を返し、足りなければ I/O を発行せず
    /// `Ok(None)` を返す（呼び出し側が [`fill_read_buf`](Self::fill_read_buf) で追加読み込み
    /// してから再試行する）。HTTP/2 多重化メインループ（F-116）は本 API で「バッファに
    /// 溜まった複数フレームを I/O なしで連続処理」する。
    pub fn try_read_frame_buffered(&mut self) -> Http2Result<Option<Frame>> {
        // フレームヘッダー (9 bytes) が未達なら不完全
        if self.buf_end - self.buf_start < FrameHeader::SIZE {
            return Ok(None);
        }

        // ヘッダーをデコード
        let header = self
            .frame_decoder
            .decode_header(&self.read_buf[self.buf_start..])?;
        let total_len = FrameHeader::SIZE + header.length as usize;

        // ペイロードが未達なら不完全
        if self.buf_end - self.buf_start < total_len {
            return Ok(None);
        }

        // フレームをデコード (安全なスライスアクセス)
        let payload_start = self.buf_start + FrameHeader::SIZE;
        let payload_end = self.buf_start + total_len;

        // バッファ境界チェック
        if payload_end > self.buf_end || payload_end > self.read_buf.len() {
            return Err(Http2Error::InvalidFrame(format!(
                "Buffer underflow: expected {} bytes, available {}",
                total_len,
                self.buf_end - self.buf_start
            )));
        }

        let payload = &self.read_buf[payload_start..payload_end];
        let frame = self.frame_decoder.decode(&header, payload)?;

        self.buf_start += total_len;

        // バッファをコンパクト化
        if self.buf_start > 32768 {
            self.compact_buffer();
        }

        Ok(Some(frame))
    }

    /// 追加データを読み込みバッファへ 1 回だけ読み込む（読み込んだバイト数を返す）。
    ///
    /// `stream.read` をちょうど 1 回発行する。0 バイト（EOF）は従来どおり
    /// `ConnectionClosed` エラー。バッファ不足時のコンパクト化・拡張（最大フレームサイズ +
    /// ヘッダー + マージン）は従来の `read_more` と同一。
    pub async fn fill_read_buf(&mut self) -> Http2Result<usize> {
        // バッファが不足している場合は拡張
        if self.buf_end >= self.read_buf.len() {
            if self.buf_start > 0 {
                self.compact_buffer();
            } else {
                // バッファを拡張 - 最大フレームサイズ + ヘッダー + マージンを確保
                let min_capacity =
                    self.frame_decoder.max_frame_size() as usize + FrameHeader::SIZE + 1024;
                let new_capacity = std::cmp::max(self.read_buf.len() * 2, min_capacity);
                self.read_buf.resize(new_capacity, 0);
            }
        }

        // 読み込み用のスライスを準備 (バッファの末尾に追加)
        // read_buf全体を渡すと0から上書きされてしまうため、split_offで後半を取り出す
        // しかしVecの所有権を渡す必要があるため、一度takeして分割し、戻ってきたら結合する

        let mut full_buf = std::mem::take(&mut self.read_buf);

        // buf_end 以降の部分を切り出して読み込み先にする（既存データを上書きしない）。
        let tail_buf = full_buf.split_off(self.buf_end);

        // 読み込み実行
        let (result, returned_tail) = self.stream.read(tail_buf).await;

        match result {
            Ok(0) => {
                self.read_buf = full_buf;
                Err(Http2Error::ConnectionClosed)
            }
            Ok(n) => {
                // 実際に読み込んだ n バイトのみ結合する。返却バッファの len は
                // IoBufMut::set_init が grow-only のため（compact 後の残留データを含み）
                // n より大きくなり得る。必ず result の n を使う（さもないと残留バイトを
                // 取り込みフレーム解析が壊れて "Frame too large" になる）。
                full_buf.extend_from_slice(&returned_tail[..n]);
                self.read_buf = full_buf;
                self.buf_end += n;
                Ok(n)
            }
            Err(e) => {
                self.read_buf = full_buf;
                Err(Http2Error::Io(e))
            }
        }
    }

    /// バッファをコンパクト化
    fn compact_buffer(&mut self) {
        if self.buf_start > 0 {
            let remaining = self.buf_end - self.buf_start;
            self.read_buf.copy_within(self.buf_start..self.buf_end, 0);
            self.buf_start = 0;
            self.buf_end = remaining;
        }
    }

    /// データを送信（所有バッファを io_uring stream へ直接委譲）
    ///
    /// runtime の `write_all`（`AsyncWriteRentExt`）は所有バッファ（`IoBuf`）を取り
    /// 完了時に返す。フレームエンコード結果の `Vec<u8>` をそのままムーブで渡すことで、
    /// 従来の per-frame `data[..].to_vec()` に由来するアロケーション + 全コピーを排除する
    /// （HTTP/2 送信ホットパス最適化）。runtime の write_all は short write を内部で
    /// 継続する（B-27）ため、`Ok` は常に完全書き込みを意味する。
    async fn write_all(&mut self, buf: Vec<u8>) -> Http2Result<()> {
        // 呼び出し境界では write_buf は空である不変条件（各送信 API は復帰前に flush する）。
        // 直接 write_all する制御フレーム等が連結バッファを追い越して順序が壊れないよう保証する。
        debug_assert!(
            self.write_buf.is_empty(),
            "write_all called with pending coalesced write_buf"
        );
        let returned = self.write_all_raw(buf).await?;
        // 所有バッファの容量を再利用のため回収（write_buf が空のときのみ）。
        if self.write_buf.is_empty() && returned.capacity() > self.write_buf.capacity() {
            self.write_buf = returned;
            self.write_buf.clear();
        }
        Ok(())
    }

    /// 所有バッファを io_uring stream へ書き込み、完了後にバッファを返す（容量再利用用）。
    ///
    /// runtime の write_all は「全書き込み or WriteZero」のため、`Ok` は常に完全書き込みを
    /// 意味する。`WouldBlock` のみ同一バッファで再試行する。
    async fn write_all_raw(&mut self, mut buf: Vec<u8>) -> Http2Result<Vec<u8>> {
        loop {
            let (result, returned) = self.stream.write_all(buf).await;
            match result {
                Ok(_) => return Ok(returned),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    buf = returned;
                    continue;
                }
                Err(e) => return Err(Http2Error::Io(e)),
            }
        }
    }

    /// 連結バッファ `write_buf` に溜まったフレームを 1 回の書き込みで送出する。
    ///
    /// 送信ホットパスの HEADERS/DATA/トレイラーを 1 本にまとめて io_uring 書き込み回数を
    /// 削減する。バッファは書き込み後に空にして容量を再利用する。
    ///
    /// F-116: 多重化メインループが `drive_streams` 後に 1 イテレーション 1 回、および
    /// `queue_data_frames` で `write_buf` が閾値超過した際に明示フラッシュするため `pub`。
    pub async fn flush_write_buf(&mut self) -> Http2Result<()> {
        if self.write_buf.is_empty() {
            return Ok(());
        }
        let buf = std::mem::take(&mut self.write_buf);
        let mut returned = self.write_all_raw(buf).await?;
        returned.clear();
        self.write_buf = returned;
        Ok(())
    }

    /// フレームを処理（外部からアクセス可能）
    ///
    /// 受信したフレームを処理し、リクエストが完了した場合は ProcessedRequest を返します。
    pub async fn process_frame(&mut self, frame: Frame) -> Http2Result<Option<ProcessedRequest>> {
        // RFC 7540 Section 4.3: ヘッダーブロック受信中は CONTINUATION のみ許可
        if let Some(pending_stream_id) = self.streams.receiving_headers_stream() {
            match &frame {
                Frame::Continuation { stream_id, .. } if *stream_id == pending_stream_id => {
                    // 正しい CONTINUATION - 処理を続行
                }
                _ => {
                    return Err(Http2Error::connection_error(
                        Http2ErrorCode::ProtocolError,
                        "Expected CONTINUATION frame during header block",
                    ));
                }
            }
        }

        match frame {
            Frame::Settings { ack, settings } => {
                self.handle_settings(ack, &settings).await?;
                Ok(None)
            }
            Frame::Headers {
                stream_id,
                end_stream,
                end_headers,
                priority,
                header_block,
            } => {
                // RFC 7540 Section 5.3.1: 自己依存チェック
                if let Some(ref p) = priority {
                    if p.dependency == stream_id {
                        return Err(Http2Error::stream_error(
                            stream_id,
                            Http2ErrorCode::ProtocolError,
                            "Stream cannot depend on itself",
                        ));
                    }
                }
                self.handle_headers(stream_id, end_stream, end_headers, priority, &header_block)
                    .await
            }
            Frame::Data {
                stream_id,
                end_stream,
                data,
            } => {
                // RFC 7540 Section 5.1: DATA on idle stream = connection error
                self.validate_stream_not_idle(stream_id, "DATA")?;
                self.handle_data(stream_id, end_stream, &data).await
            }
            Frame::WindowUpdate {
                stream_id,
                increment,
            } => {
                // RFC 7540 Section 5.1: WINDOW_UPDATE on idle stream = connection error
                if stream_id != 0 {
                    self.validate_stream_not_idle(stream_id, "WINDOW_UPDATE")?;
                }
                self.handle_window_update(stream_id, increment)?;
                Ok(None)
            }
            Frame::Ping { ack, data } => {
                self.handle_ping(ack, &data).await?;
                Ok(None)
            }
            Frame::GoAway {
                last_stream_id,
                error_code,
                debug_data,
            } => {
                self.handle_goaway(last_stream_id, error_code, &debug_data)?;
                Ok(None)
            }
            Frame::RstStream {
                stream_id,
                error_code,
            } => {
                // RFC 7540 Section 6.4: RST_STREAM on stream 0 is connection error
                if stream_id == 0 {
                    return Err(Http2Error::protocol_error("RST_STREAM with stream ID 0"));
                }
                // RFC 7540 Section 5.1: RST_STREAM on idle stream = connection error
                // A stream is truly idle if it has never been opened (stream_id > max seen)
                // We accept RST_STREAM on previously opened streams even if cleaned up
                if self.streams.get_ref(stream_id).is_none()
                    && stream_id > self.streams.max_client_stream_id()
                {
                    return Err(Http2Error::connection_error(
                        Http2ErrorCode::ProtocolError,
                        format!("RST_STREAM on idle stream {}", stream_id),
                    ));
                }
                self.handle_rst_stream(stream_id, error_code)?;
                Ok(None)
            }
            Frame::Priority {
                stream_id,
                priority,
            } => {
                // RFC 7540 Section 5.3.1: 自己依存チェック
                if priority.dependency == stream_id {
                    return Err(Http2Error::stream_error(
                        stream_id,
                        Http2ErrorCode::ProtocolError,
                        "Stream cannot depend on itself",
                    ));
                }
                self.handle_priority(stream_id, priority)?;
                Ok(None)
            }
            Frame::Continuation {
                stream_id,
                end_headers,
                header_block,
            } => {
                self.handle_continuation(stream_id, end_headers, &header_block)
                    .await
            }
            Frame::PushPromise { .. } => {
                // クライアントからの PUSH_PROMISE は無効
                Err(Http2Error::protocol_error("Client sent PUSH_PROMISE"))
            }
            Frame::Unknown { .. } => {
                // 未知のフレームは無視 (RFC 7540 Section 4.1)
                Ok(None)
            }
        }
    }

    /// ストリームがアイドル状態でないことを検証 (RFC 7540 Section 5.1)
    fn validate_stream_not_idle(&self, stream_id: u32, frame_type: &str) -> Http2Result<()> {
        if self.streams.get_ref(stream_id).is_none() {
            // ストリームが存在しない = idle 状態
            return Err(Http2Error::connection_error(
                Http2ErrorCode::ProtocolError,
                format!("{} frame on idle stream {}", frame_type, stream_id),
            ));
        }
        Ok(())
    }

    /// SETTINGS フレームを処理 (Control Frame Flooding 対策付き)
    async fn handle_settings(&mut self, ack: bool, settings: &[(u16, u32)]) -> Http2Result<()> {
        if ack {
            // ACK を受信
            self.settings_ack_pending = false;
            return Ok(());
        }

        // レート制限チェック (非ACK の SETTINGS フレーム)
        self.check_control_frame_rate()?;

        // クライアントの設定を適用
        for &(id, value) in settings {
            match id {
                0x1 => {
                    // HEADER_TABLE_SIZE
                    // RFC 7540 Section 6.5.2: This informs our encoder of the remote decoder's limit
                    self.hpack_encoder.set_max_table_size(value as usize);
                    // Note: Our hpack_decoder limit is set when we send our own SETTINGS to the remote.
                }
                0x2 => {
                    // ENABLE_PUSH - RFC 7540 Section 6.5.2: 0 または 1 のみ有効
                    if value > 1 {
                        return Err(Http2Error::connection_error(
                            Http2ErrorCode::ProtocolError,
                            "ENABLE_PUSH must be 0 or 1",
                        ));
                    }
                    // サーバーでは ENABLE_PUSH の値自体は使用しない
                }
                0x3 => {
                    // MAX_CONCURRENT_STREAMS
                    // クライアントが許可する最大ストリーム数（サーバーからのプッシュ用）
                }
                0x4 => {
                    // INITIAL_WINDOW_SIZE
                    if value > 0x7FFFFFFF {
                        return Err(Http2Error::connection_error(
                            Http2ErrorCode::FlowControlError,
                            "INITIAL_WINDOW_SIZE too large",
                        ));
                    }
                    self.streams.update_initial_window_size(value as i32)?;
                    self.remote_settings.initial_window_size = value;
                }
                0x5 => {
                    // MAX_FRAME_SIZE
                    if !(defaults::MAX_FRAME_SIZE..=defaults::MAX_FRAME_SIZE_UPPER_LIMIT)
                        .contains(&value)
                    {
                        return Err(Http2Error::protocol_error("Invalid MAX_FRAME_SIZE"));
                    }
                    self.frame_encoder.set_max_frame_size(value);
                    // RFC 7540 Section 6.5.2:
                    // SETTINGS_MAX_FRAME_SIZE indicates the sender's maximum frame size.
                    // It does NOT affect our receiving limit (which is local_settings.max_frame_size).
                    // So we do NOT update frame_decoder.max_frame_size here.
                    self.remote_settings.max_frame_size = value;
                }
                0x6 => {
                    // MAX_HEADER_LIST_SIZE
                    self.remote_settings.max_header_list_size = value;
                }
                _ => {
                    // 未知の設定は無視
                }
            }
        }

        // SETTINGS ACK を送信
        let ack_frame = self.frame_encoder.encode_settings_ack();
        self.write_all(ack_frame).await?;

        Ok(())
    }

    /// HEADERS フレームを処理 (CONTINUATION Flood 対策付き)
    ///
    /// CVE-2024-24786 対策として、ヘッダーブロックサイズと CONTINUATION フレーム数を制限。
    async fn handle_headers(
        &mut self,
        stream_id: u32,
        end_stream: bool,
        end_headers: bool,
        priority: Option<crate::http2::frame::types::PrioritySpec>,
        header_block: &[u8],
    ) -> Http2Result<Option<ProcessedRequest>> {
        // CONTINUATION 中に他のフレームを受信したらエラー
        if let Some(pending_id) = self.streams.receiving_headers_stream() {
            if pending_id != stream_id {
                return Err(Http2Error::protocol_error("Expected CONTINUATION frame"));
            }
        }

        // CONTINUATION カウンターをリセット (新しいヘッダーブロック開始)
        self.continuation_count = 0;

        // ヘッダーブロックサイズチェック (HPACK Bomb 対策)
        if header_block.len() > self.local_settings.max_header_block_size {
            ftlog::warn!(
                "[HTTP/2] Header block too large: {} bytes (limit: {})",
                header_block.len(),
                self.local_settings.max_header_block_size
            );
            return Err(Http2Error::stream_error(
                stream_id,
                Http2ErrorCode::EnhanceYourCalm,
                "Header block size limit exceeded",
            ));
        }

        // Check if this is a trailer (second HEADERS on existing stream)
        // Trailers MUST have END_STREAM set (RFC 7540 §8.1)
        let is_trailer = if let Some(stream) = self.streams.get_ref(stream_id) {
            // Stream exists - this is a trailer if:
            // 1. Stream is in Open or HalfClosedLocal state (headers already received)
            // 2. And this HEADERS has END_STREAM set
            matches!(
                stream.state,
                StreamState::Open | StreamState::HalfClosedLocal
            ) && end_stream
        } else {
            false
        };

        // RFC 7540 §8.1: A second HEADERS frame without END_STREAM is a protocol error
        // (except for trailers which must have END_STREAM)
        if let Some(stream) = self.streams.get_ref(stream_id) {
            if matches!(
                stream.state,
                StreamState::Open | StreamState::HalfClosedLocal
            ) && !end_stream
            {
                // Second HEADERS without END_STREAM - must be a protocol error
                return Err(Http2Error::stream_error(
                    stream_id,
                    Http2ErrorCode::ProtocolError,
                    "Second HEADERS frame without END_STREAM",
                ));
            }
        }

        // ストリームを取得または作成
        // エラー発生時（例: 同時ストリーム数制限超過）はRST_STREAMを送信
        let stream_result = self.streams.get_or_create_client_stream(stream_id);

        let stream = match stream_result {
            Ok(s) => s,
            Err(e) => {
                // エラーが発生した場合、RST_STREAMを送信
                // ストリームが作成されていない場合でも、ストリームIDは有効
                if let Some(id) = e.rst_stream_id() {
                    let _ = self.send_rst_stream(id, e.error_code()).await;
                }
                return Err(e);
            }
        };

        // 状態遷移
        stream.recv_headers(end_stream)?;

        // Priority を設定
        if let Some(p) = priority {
            stream.dependency = p.dependency;
            stream.weight = p.weight;
            stream.exclusive = p.exclusive;
        }

        // ヘッダーブロックを追加
        stream.append_header_fragment(header_block, end_headers);

        if end_headers {
            self.streams.set_receiving_headers(None);
            self.continuation_count = 0; // リセット
            self.decode_and_set_headers(stream_id, is_trailer)?;

            // リクエストが完了したかチェック
            if end_stream {
                return Ok(Some(ProcessedRequest {
                    stream_id,
                    body_pending: false,
                }));
            } else if !is_trailer {
                // ヘッダー完了・ボディ継続: 呼び出し側にストリーミング判断の機会を与える
                // （F-32）。適格でなければ呼び出し側は無視し、DATA は request_body へ蓄積される。
                return Ok(Some(ProcessedRequest {
                    stream_id,
                    body_pending: true,
                }));
            }
        } else {
            // CONTINUATION が続く場合、receiving_headers_stream を設定
            self.streams.set_receiving_headers(Some(stream_id));
        }

        Ok(None)
    }

    /// CONTINUATION フレームを処理 (CONTINUATION Flood 対策付き)
    ///
    /// CVE-2024-24786 対策として、CONTINUATION フレーム数と累積ヘッダーブロックサイズを制限。
    async fn handle_continuation(
        &mut self,
        stream_id: u32,
        end_headers: bool,
        header_block: &[u8],
    ) -> Http2Result<Option<ProcessedRequest>> {
        // CONTINUATION 中でなければエラー
        let pending_id = self
            .streams
            .receiving_headers_stream()
            .ok_or_else(|| Http2Error::protocol_error("Unexpected CONTINUATION"))?;

        if pending_id != stream_id {
            return Err(Http2Error::protocol_error("CONTINUATION for wrong stream"));
        }

        // CONTINUATION フレーム数チェック (CONTINUATION Flood 対策)
        self.continuation_count += 1;
        if self.continuation_count > self.local_settings.max_continuation_frames {
            ftlog::warn!(
                "[HTTP/2] CONTINUATION Flood detected: {} frames (limit: {})",
                self.continuation_count,
                self.local_settings.max_continuation_frames
            );
            return Err(Http2Error::connection_error(
                Http2ErrorCode::EnhanceYourCalm,
                "CONTINUATION frame limit exceeded",
            ));
        }

        // ストリームを取得 - CONTINUATION中はストリームが必ず存在するはず
        // RFC 7540: ストリームが見つからない場合は接続エラー
        let stream = self
            .streams
            .get(stream_id)
            .ok_or_else(|| Http2Error::protocol_error("Stream not found during CONTINUATION"))?;

        // 累積ヘッダーブロックサイズチェック (HPACK Bomb 対策)
        let current_size = stream.pending_header_len();
        let new_size = current_size + header_block.len();
        if new_size > self.local_settings.max_header_block_size {
            ftlog::warn!(
                "[HTTP/2] Cumulative header block too large: {} bytes (limit: {})",
                new_size,
                self.local_settings.max_header_block_size
            );
            return Err(Http2Error::stream_error(
                stream_id,
                Http2ErrorCode::EnhanceYourCalm,
                "Cumulative header block size limit exceeded",
            ));
        }

        // end_stream: HalfClosedRemote means END_STREAM was set on HEADERS
        let end_stream = matches!(
            stream.state,
            StreamState::HalfClosedRemote | StreamState::Closed
        );

        // Trailers: A CONTINUATION is for trailers ONLY if we already have decoded request headers.
        // Just being HalfClosedRemote is NOT sufficient - that could just mean HEADERS had END_STREAM
        // but END_HEADERS will come in CONTINUATION (normal case for split headers).
        let is_trailer = !stream.request_headers.is_empty();

        stream.append_header_fragment(header_block, end_headers);

        if end_headers {
            self.streams.set_receiving_headers(None);
            self.continuation_count = 0; // リセット
            self.decode_and_set_headers(stream_id, is_trailer)?;

            if end_stream {
                return Ok(Some(ProcessedRequest {
                    stream_id,
                    body_pending: false,
                }));
            } else if !is_trailer {
                // 分割ヘッダー完了・ボディ継続: ストリーミング起点（F-32）
                return Ok(Some(ProcessedRequest {
                    stream_id,
                    body_pending: true,
                }));
            }
        }

        Ok(None)
    }

    /// ヘッダーブロックをデコードしてストリームに設定
    fn decode_and_set_headers(&mut self, stream_id: u32, is_trailer: bool) -> Http2Result<()> {
        let stream = self.streams.get(stream_id).ok_or_else(|| {
            Http2Error::stream_error(stream_id, Http2ErrorCode::StreamClosed, "Stream not found")
        })?;

        let header_block = stream.take_header_block();
        let headers = self.hpack_decoder.decode(&header_block).map_err(|e| {
            // HPACKエラーを適切に処理
            ftlog::warn!(
                "[HTTP/2] HPACK decode error for stream {}: {}",
                stream_id,
                e
            );
            Http2Error::compression_error(format!("HPACK decode error: {}", e))
        })?;

        // ヘッダーを検証 (RFC 7540 Section 8.1.2)
        Self::validate_request_headers(&headers, stream_id, is_trailer)?;

        let stream = self.streams.get(stream_id).unwrap();

        // For trailers, we don't overwrite request_headers but could store them separately
        // For now, trailers just need to pass validation
        if !is_trailer {
            stream.request_headers = headers;

            // Content-Length を解析
            if let Some(cl) = stream
                .request_headers
                .iter()
                .find(|h| h.name == b"content-length")
            {
                if let Some(len) = std::str::from_utf8(&cl.value)
                    .ok()
                    .and_then(|s| s.parse::<u64>().ok())
                {
                    stream.content_length = Some(len);
                }
            }
        }

        Ok(())
    }

    /// リクエストヘッダーを検証 (RFC 7540 Section 8.1.2)
    ///
    /// バリデーション項目:
    /// - ヘッダー名の大文字チェック (8.1.2)
    /// - 擬似ヘッダーの順序 (8.1.2.1)
    /// - 必須擬似ヘッダーの存在確認 (8.1.2.3)
    /// - 擬似ヘッダーの重複チェック (8.1.2.3)
    /// - 接続固有ヘッダーの禁止 (8.1.2.2)
    /// - TE ヘッダーの値チェック (8.1.2.2)
    fn validate_request_headers(
        headers: &[crate::http2::hpack::HeaderField],
        stream_id: u32,
        is_trailer: bool,
    ) -> Http2Result<()> {
        let mut seen_regular = false;
        let mut method_count = 0u8;
        let mut scheme_count = 0u8;
        let mut path_count = 0u8;
        let mut authority_count = 0u8;

        for header in headers {
            let name = &header.name;

            // ヘッダー名に大文字が含まれていないことを確認 (RFC 7540 8.1.2)
            if name.iter().any(|&b| b.is_ascii_uppercase()) {
                return Err(Http2Error::stream_error(
                    stream_id,
                    Http2ErrorCode::ProtocolError,
                    "Header name contains uppercase characters",
                ));
            }

            if name.starts_with(b":") {
                // 擬似ヘッダーの処理

                // トレーラーに擬似ヘッダーは禁止 (RFC 7540 8.1.2.1)
                if is_trailer {
                    return Err(Http2Error::stream_error(
                        stream_id,
                        Http2ErrorCode::ProtocolError,
                        "Pseudo-header in trailer",
                    ));
                }

                // 通常ヘッダーの後に擬似ヘッダーは禁止 (RFC 7540 8.1.2.1)
                if seen_regular {
                    return Err(Http2Error::stream_error(
                        stream_id,
                        Http2ErrorCode::ProtocolError,
                        "Pseudo-header after regular header",
                    ));
                }

                match name.as_slice() {
                    b":method" => {
                        method_count += 1;
                    }
                    b":scheme" => {
                        scheme_count += 1;
                    }
                    b":path" => {
                        path_count += 1;
                        // 空の :path は禁止 (RFC 7540 8.1.2.3)
                        if header.value.is_empty() {
                            return Err(Http2Error::stream_error(
                                stream_id,
                                Http2ErrorCode::ProtocolError,
                                "Empty :path pseudo-header",
                            ));
                        }
                    }
                    b":authority" => {
                        authority_count += 1;
                    }
                    b":status" => {
                        // リクエストにレスポンス用擬似ヘッダーは禁止 (RFC 7540 8.1.2.1)
                        return Err(Http2Error::stream_error(
                            stream_id,
                            Http2ErrorCode::ProtocolError,
                            "Response pseudo-header :status in request",
                        ));
                    }
                    _ => {
                        // 未知の擬似ヘッダーは禁止 (RFC 7540 8.1.2.1)
                        return Err(Http2Error::stream_error(
                            stream_id,
                            Http2ErrorCode::ProtocolError,
                            "Unknown pseudo-header",
                        ));
                    }
                }
            } else {
                // 通常ヘッダーの処理
                seen_regular = true;

                // 接続固有ヘッダーの禁止 (RFC 7540 8.1.2.2)
                let lower = name.to_ascii_lowercase();
                match lower.as_slice() {
                    b"connection" | b"keep-alive" | b"proxy-connection" | b"transfer-encoding"
                    | b"upgrade" => {
                        return Err(Http2Error::stream_error(
                            stream_id,
                            Http2ErrorCode::ProtocolError,
                            "Connection-specific header field",
                        ));
                    }
                    b"te" => {
                        // TE ヘッダーは "trailers" 以外禁止 (RFC 7540 8.1.2.2)
                        if !header.value.eq_ignore_ascii_case(b"trailers") {
                            return Err(Http2Error::stream_error(
                                stream_id,
                                Http2ErrorCode::ProtocolError,
                                "TE header with value other than 'trailers'",
                            ));
                        }
                    }
                    _ => {}
                }
            }
        }

        // 必須擬似ヘッダーの確認 (RFC 7540 8.1.2.3)
        // トレーラーでは擬似ヘッダーは不要
        if !is_trailer {
            // :method, :scheme, :path は必須かつ1つのみ
            if method_count != 1 {
                return Err(Http2Error::stream_error(
                    stream_id,
                    Http2ErrorCode::ProtocolError,
                    if method_count == 0 {
                        "Missing :method pseudo-header"
                    } else {
                        "Duplicate :method pseudo-header"
                    },
                ));
            }
            if scheme_count != 1 {
                return Err(Http2Error::stream_error(
                    stream_id,
                    Http2ErrorCode::ProtocolError,
                    if scheme_count == 0 {
                        "Missing :scheme pseudo-header"
                    } else {
                        "Duplicate :scheme pseudo-header"
                    },
                ));
            }
            if path_count != 1 {
                return Err(Http2Error::stream_error(
                    stream_id,
                    Http2ErrorCode::ProtocolError,
                    if path_count == 0 {
                        "Missing :path pseudo-header"
                    } else {
                        "Duplicate :path pseudo-header"
                    },
                ));
            }
            // :authority は任意だが複数は禁止
            if authority_count > 1 {
                return Err(Http2Error::stream_error(
                    stream_id,
                    Http2ErrorCode::ProtocolError,
                    "Duplicate :authority pseudo-header",
                ));
            }
        }

        Ok(())
    }

    /// DATA フレームを処理
    async fn handle_data(
        &mut self,
        stream_id: u32,
        end_stream: bool,
        data: &[u8],
    ) -> Http2Result<Option<ProcessedRequest>> {
        // コネクションレベルフロー制御
        let data_len = data.len() as i32;
        if data_len > self.conn_recv_window {
            return Err(Http2Error::connection_error(
                Http2ErrorCode::FlowControlError,
                "Connection flow control window exceeded",
            ));
        }
        self.conn_recv_window -= data_len;

        // ストリームレベルフロー制御
        let stream = self.streams.get(stream_id).ok_or_else(|| {
            Http2Error::stream_error(stream_id, Http2ErrorCode::StreamClosed, "Stream not found")
        })?;

        // アクティビティ更新 (Slow Loris 対策)
        stream.update_activity();

        stream.recv_data(data, end_stream)?;

        // WINDOW_UPDATE を送信 (必要に応じて)
        self.maybe_send_window_update(stream_id).await?;

        if end_stream {
            Ok(Some(ProcessedRequest {
                stream_id,
                body_pending: false,
            }))
        } else {
            Ok(None)
        }
    }

    /// ストリーミング転送用の DATA 受信処理（F-32 リクエスト方向ストリーミング）
    ///
    /// 通常の DATA 処理（`process_frame` → `handle_data`）と同等のフロー制御・
    /// 状態遷移・content-length 検証を行うが、受信データを `Stream::request_body` へ
    /// **バッファしない**。呼び出し側（proxy）は受信した DATA フレームの所有バッファを
    /// そのままゼロコピーでバックエンドへ転送できる。
    ///
    /// **WINDOW_UPDATE（recv ウィンドウ補充）は行わない**（F-116）。受信時に即補充すると
    /// クライアントが常に新規クレジットを得て送信し続けられ、バックエンドが遅い場合に
    /// 未転送ボディが際限なく滞留する（バックプレッシャ喪失）。補充は呼び出し側が
    /// **下流へ消費できたタイミング**で [`replenish_recv_window`](Self::replenish_recv_window)
    /// により行うこと（旧 F-32 経路の「バックエンド書き込み後に補充」と同方針）。
    ///
    /// `data_len` には受信した DATA ペイロード長を渡す。`process_frame` の DATA アーム
    /// と同じ事前検証（ヘッダーブロック受信中でないこと・idle ストリームでないこと）を行う。
    pub fn recv_data_for_streaming(
        &mut self,
        stream_id: u32,
        end_stream: bool,
        data_len: usize,
    ) -> Http2Result<()> {
        // RFC 7540 §4.3: ヘッダーブロック受信中は CONTINUATION 以外を受け付けない
        if self.streams.receiving_headers_stream().is_some() {
            return Err(Http2Error::connection_error(
                Http2ErrorCode::ProtocolError,
                "Expected CONTINUATION frame during header block",
            ));
        }
        // RFC 7540 §5.1: idle ストリームへの DATA は接続エラー
        self.validate_stream_not_idle(stream_id, "DATA")?;

        // コネクションレベルフロー制御
        let dl = data_len as i32;
        if dl > self.conn_recv_window {
            return Err(Http2Error::connection_error(
                Http2ErrorCode::FlowControlError,
                "Connection flow control window exceeded",
            ));
        }
        self.conn_recv_window -= dl;

        // ストリームレベル: アカウンティング（バッファリングなし）
        let stream = self.streams.get(stream_id).ok_or_else(|| {
            Http2Error::stream_error(stream_id, Http2ErrorCode::StreamClosed, "Stream not found")
        })?;
        stream.update_activity();
        stream.recv_data_accounting(data_len, end_stream)?;

        Ok(())
    }

    /// recv ウィンドウの消費連動補充（F-116。必要に応じて WINDOW_UPDATE を **write_buf へ積む**）。
    ///
    /// [`recv_data_for_streaming`](Self::recv_data_for_streaming) で消費した recv ウィンドウを、
    /// 受信ボディを下流（req チャネル→バックエンド）へ転送できたタイミングで補充する。
    /// 閾値（ウィンドウ半分超の消費）は `maybe_send_window_update` と同一。ストリームが既に
    /// 存在しない場合はコネクションレベルのみ補充する（ストリーム打ち切り時のクレジット
    /// リーク防止に使える）。
    ///
    /// `write_all` ではなく連結バッファ `write_buf` へ追記する（await しない）。多重化
    /// メインループの drive 中は `write_buf` に他ストリームのフレームが積まれていることが
    /// あり、直接 `write_all` すると順序が壊れるため。送出は次回の `flush_write_buf` で行われる。
    pub fn replenish_recv_window(&mut self, stream_id: u32) {
        // コネクションレベル
        let conn_increment = defaults::CONNECTION_WINDOW_SIZE as i32 - self.conn_recv_window;
        if conn_increment > (defaults::CONNECTION_WINDOW_SIZE as i32 / 2) {
            let frame = self
                .frame_encoder
                .encode_window_update(0, conn_increment as u32);
            self.write_buf.extend_from_slice(&frame);
            self.conn_recv_window += conn_increment;
        }

        // ストリームレベル（ストリーム消滅後はスキップ）
        let stream_increment = if let Some(stream) = self.streams.get(stream_id) {
            let increment = self.local_settings.initial_window_size as i32 - stream.recv_window;
            if increment > (self.local_settings.initial_window_size as i32 / 2) {
                Some(increment)
            } else {
                None
            }
        } else {
            None
        };
        if let Some(increment) = stream_increment {
            let frame = self
                .frame_encoder
                .encode_window_update(stream_id, increment as u32);
            self.write_buf.extend_from_slice(&frame);
            if let Some(stream) = self.streams.get(stream_id) {
                stream.update_recv_window(increment);
            }
        }
    }

    /// WINDOW_UPDATE を送信 (必要に応じて)
    async fn maybe_send_window_update(&mut self, stream_id: u32) -> Http2Result<()> {
        // コネクションレベル
        let conn_increment = defaults::CONNECTION_WINDOW_SIZE as i32 - self.conn_recv_window;
        if conn_increment > (defaults::CONNECTION_WINDOW_SIZE as i32 / 2) {
            let frame = self
                .frame_encoder
                .encode_window_update(0, conn_increment as u32);
            self.write_all(frame).await?;
            self.conn_recv_window += conn_increment;
        }

        // ストリームレベル
        let stream_increment = if let Some(stream) = self.streams.get(stream_id) {
            let increment = self.local_settings.initial_window_size as i32 - stream.recv_window;
            if increment > (self.local_settings.initial_window_size as i32 / 2) {
                Some(increment)
            } else {
                None
            }
        } else {
            None
        };

        if let Some(increment) = stream_increment {
            let frame = self
                .frame_encoder
                .encode_window_update(stream_id, increment as u32);
            self.write_all(frame).await?;
            if let Some(stream) = self.streams.get(stream_id) {
                stream.update_recv_window(increment);
            }
        }

        Ok(())
    }

    /// WINDOW_UPDATE を処理
    fn handle_window_update(&mut self, stream_id: u32, increment: u32) -> Http2Result<()> {
        if stream_id == 0 {
            // コネクションレベル
            let new_window = self.conn_send_window.checked_add(increment as i32);
            match new_window {
                Some(w) if (w as i64) <= 0x7FFFFFFF => {
                    self.conn_send_window = w;
                }
                _ => {
                    return Err(Http2Error::connection_error(
                        Http2ErrorCode::FlowControlError,
                        "Connection window overflow",
                    ));
                }
            }
        } else {
            // ストリームレベル
            if let Some(stream) = self.streams.get(stream_id) {
                stream.recv_window_update(increment)?;
            }
        }

        Ok(())
    }

    /// PING を処理 (Control Frame Flooding 対策付き)
    ///
    /// 制御フレームのレート制限を適用し、フラッド攻撃を防止。
    async fn handle_ping(&mut self, ack: bool, data: &[u8; 8]) -> Http2Result<()> {
        // レート制限チェック (ACK でない場合のみカウント)
        if !ack {
            self.check_control_frame_rate()?;

            // PING ACK を送信
            let frame = self.frame_encoder.encode_ping(data, true);
            self.write_all(frame).await?;
        }
        Ok(())
    }

    pub async fn send_response(
        &mut self,
        stream_id: u32,
        status: u16,
        headers: &[(&[u8], &[u8])],
        body: Option<&[u8]>,
    ) -> Http2Result<()> {
        let mut lowercase_names: Vec<Vec<u8>> = Vec::with_capacity(headers.len());
        for &(name, _) in headers {
            lowercase_names.push(name.to_ascii_lowercase());
        }

        let empty_body = body.is_none() || body.map(|b| b.is_empty()).unwrap_or(true);

        // HEADERS を連結バッファへ積む。ボディがあるときはフラッシュせず、続く DATA と
        // 1 回の書き込みにまとめる（送信ホットパスのシステムコール削減・F-73 続き）。
        self.send_headers_internal(
            stream_id,
            status,
            headers,
            &lowercase_names,
            empty_body,
            empty_body, // flush: ボディ無しなら即フラッシュ
        )
        .await?;

        if let Some(body_data) = body {
            if !body_data.is_empty() {
                self.send_data(stream_id, body_data, true).await?;
            }
        }

        Ok(())
    }

    /// ヘッダーのみを送信 (ステータスコード付き)
    pub async fn send_headers(
        &mut self,
        stream_id: u32,
        status: u16,
        headers: &[(&[u8], &[u8])],
        end_stream: bool,
    ) -> Http2Result<()> {
        let mut lowercase_names: Vec<Vec<u8>> = Vec::with_capacity(headers.len());
        for &(name, _) in headers {
            lowercase_names.push(name.to_ascii_lowercase());
        }

        self.send_headers_internal(
            stream_id,
            status,
            headers,
            &lowercase_names,
            end_stream,
            true,
        )
        .await
    }

    /// ヘッダーを連結バッファへ積む（`end_stream` 指定可能）。即送出しない（F-116）。
    ///
    /// 多重化メインループの `drive_streams` が、複数ストリームの HEADERS/DATA を
    /// `write_buf` へ合流させ 1 回の書き込みでフラッシュするために使う。`end_stream=true` で
    /// ボディ無し応答（リダイレクト・304 等）の HEADERS に END_STREAM を付与する。
    pub async fn send_headers_buffered_end(
        &mut self,
        stream_id: u32,
        status: u16,
        headers: &[(&[u8], &[u8])],
        end_stream: bool,
    ) -> Http2Result<()> {
        let mut lowercase_names: Vec<Vec<u8>> = Vec::with_capacity(headers.len());
        for &(name, _) in headers {
            lowercase_names.push(name.to_ascii_lowercase());
        }
        self.send_headers_internal(
            stream_id,
            status,
            headers,
            &lowercase_names,
            end_stream,
            false,
        )
        .await
    }

    /// 連結バッファ `write_buf` に現在積まれているバイト数（F-116）。
    ///
    /// `queue_data_frames` は決してフラッシュしないため、メインループは本値が
    /// フラッシュ閾値（128KB）を超えたら明示的に `flush_write_buf` する。
    pub fn pending_write_len(&self) -> usize {
        self.write_buf.len()
    }

    /// ヘッダーを連結バッファへ積むだけで即送出しない（ストリーミング応答の HEADERS +
    /// 最初の DATA 連結用）。
    ///
    /// 送信ホットパス最適化。呼び出し後は **必ず** 同一ストリームの `send_data`（末尾で
    /// フラッシュする）を続けて呼ぶこと。途中で `write_all` を伴う制御フレーム送出
    /// （`send_rst_stream` 等）を挟んではならない（連結バッファに HEADERS が残っているため
    /// 順序が壊れる）。`end_stream=false` 固定（ボディが続く前提）。
    pub async fn send_headers_buffered(
        &mut self,
        stream_id: u32,
        status: u16,
        headers: &[(&[u8], &[u8])],
    ) -> Http2Result<()> {
        let mut lowercase_names: Vec<Vec<u8>> = Vec::with_capacity(headers.len());
        for &(name, _) in headers {
            lowercase_names.push(name.to_ascii_lowercase());
        }

        self.send_headers_internal(stream_id, status, headers, &lowercase_names, false, false)
            .await
    }

    /// ヘッダー送信の内部実装
    ///
    /// `flush` が `true` のとき連結バッファを即座に書き込む。`false` のときは
    /// 続く DATA/トレイラーと 1 回の書き込みにまとめるためバッファに残す。
    async fn send_headers_internal(
        &mut self,
        stream_id: u32,
        status: u16,
        headers: &[(&[u8], &[u8])],
        lowercase_names: &[Vec<u8>],
        end_stream: bool,
        flush: bool,
    ) -> Http2Result<()> {
        // ステータスコードを文字列に変換
        let mut status_buf = [0u8; 3];
        let status_str: &[u8] = match status {
            200 => b"200",
            204 => b"204",
            206 => b"206",
            301 => b"301",
            302 => b"302",
            304 => b"304",
            400 => b"400",
            401 => b"401",
            403 => b"403",
            404 => b"404",
            500 => b"500",
            502 => b"502",
            503 => b"503",
            504 => b"504",
            _ => {
                let s = status.to_string();
                let b = s.as_bytes();
                let len = b.len().min(3);
                status_buf[..len].copy_from_slice(&b[..len]);
                &status_buf[..len]
            }
        };

        // ステータスとヘッダーをエンコード
        let mut header_list: Vec<(&[u8], &[u8], bool)> = Vec::with_capacity(headers.len() + 1);
        header_list.push((b":status", status_str, false));

        for (i, &(_, value)) in headers.iter().enumerate() {
            header_list.push((&lowercase_names[i], value, false));
        }

        let header_block = self
            .hpack_encoder
            .encode(&header_list)
            .map_err(|e| Http2Error::HpackEncode(e.to_string()))?;

        self.frame_encoder.encode_headers_into(
            &mut self.write_buf,
            stream_id,
            &header_block,
            end_stream,
            true, // end_headers
            None,
        );

        if flush {
            self.flush_write_buf().await?;
        }

        // ストリーム状態を更新
        if let Some(stream) = self.streams.get(stream_id) {
            stream.send_headers(end_stream)?;
        }

        Ok(())
    }

    /// 制御フレームのレート制限をチェック
    ///
    /// PING, SETTINGS, WINDOW_UPDATE(stream_id=0) などの制御フレームを
    /// 対象としてレート制限を適用。
    fn check_control_frame_rate(&mut self) -> Http2Result<()> {
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.control_frame_window_start);

        if elapsed.as_secs() >= 1 {
            // ウィンドウをリセット
            self.control_frame_count = 1;
            self.control_frame_window_start = now;
        } else {
            self.control_frame_count += 1;

            // 閾値超過チェック
            if self.control_frame_count > self.local_settings.max_control_frames_per_second {
                ftlog::warn!(
                    "[HTTP/2] Control frame flood detected: {} frames in 1 second (limit: {})",
                    self.control_frame_count,
                    self.local_settings.max_control_frames_per_second
                );
                return Err(Http2Error::connection_error(
                    Http2ErrorCode::EnhanceYourCalm,
                    "Control frame rate limit exceeded",
                ));
            }
        }

        Ok(())
    }

    /// GOAWAY を処理 (RFC 7540 Section 6.8)
    ///
    /// GOAWAY 受信後は last_stream_id より大きい ID のストリームを
    /// 開始してはならない。
    fn handle_goaway(
        &mut self,
        last_stream_id: u32,
        error_code: u32,
        debug_data: &[u8],
    ) -> Http2Result<()> {
        self.goaway_received = true;
        self.goaway_last_stream_id = Some(last_stream_id);

        // ストリームマネージャーにも GOAWAY 状態を伝播
        self.streams.set_goaway_last_stream_id(last_stream_id);

        // エラーコードが 0 以外の場合はログを出力
        if error_code != 0 {
            let debug_str = String::from_utf8_lossy(debug_data);
            ftlog::warn!(
                "HTTP/2 GOAWAY received: error_code={}, last_stream_id={}, debug={}",
                error_code,
                last_stream_id,
                debug_str
            );
        } else {
            ftlog::debug!("HTTP/2 GOAWAY received: last_stream_id={}", last_stream_id);
        }

        Ok(())
    }

    /// RST_STREAM を処理 (Rapid Reset 対策付き)
    ///
    /// CVE-2023-44487 (Rapid Reset) 対策として、RST_STREAM のレート制限を実装。
    /// 閾値を超えた場合は ENHANCE_YOUR_CALM (0xb) エラーで接続を切断。
    fn handle_rst_stream(&mut self, stream_id: u32, error_code: u32) -> Http2Result<()> {
        // レート制限チェック (Rapid Reset 対策)
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(self.rst_stream_window_start);

        if elapsed.as_secs() >= 1 {
            // ウィンドウをリセット
            self.rst_stream_count = 1;
            self.rst_stream_window_start = now;
        } else {
            self.rst_stream_count += 1;

            // 閾値超過チェック
            if self.rst_stream_count > self.local_settings.max_rst_stream_per_second {
                ftlog::warn!(
                    "[HTTP/2] Rapid Reset attack detected: {} RST_STREAM frames in 1 second (limit: {})",
                    self.rst_stream_count,
                    self.local_settings.max_rst_stream_per_second
                );
                return Err(Http2Error::connection_error(
                    Http2ErrorCode::EnhanceYourCalm,
                    "RST_STREAM rate limit exceeded",
                ));
            }
        }

        if let Some(stream) = self.streams.get(stream_id) {
            stream.recv_rst_stream(error_code);
        }
        Ok(())
    }

    /// PRIORITY を処理
    fn handle_priority(
        &mut self,
        stream_id: u32,
        priority: crate::http2::frame::types::PrioritySpec,
    ) -> Http2Result<()> {
        if let Some(stream) = self.streams.get(stream_id) {
            stream.dependency = priority.dependency;
            stream.weight = priority.weight;
            stream.exclusive = priority.exclusive;
        }
        Ok(())
    }

    /// gRPC レスポンスを送信 (トレイラー付き)
    ///
    /// gRPC プロトコルでは、レスポンス終了時に grpc-status と grpc-message を
    /// HTTP/2 トレイラーとして送信する必要があります。
    ///
    /// # 引数
    /// * `stream_id` - ストリーム ID
    /// * `headers` - レスポンスヘッダー (content-type 等)
    /// * `body` - レスポンスボディ (gRPC フレーム済み)
    /// * `grpc_status` - gRPC ステータスコード (0 = OK)
    /// * `grpc_message` - gRPC エラーメッセージ (オプション)
    #[cfg(feature = "grpc")]
    pub async fn send_grpc_response(
        &mut self,
        stream_id: u32,
        headers: &[(&[u8], &[u8])],
        body: Option<&[u8]>,
        grpc_status: u32,
        grpc_message: Option<&str>,
    ) -> Http2Result<()> {
        // 1. HEADERS フレーム送信 (200 OK + content-type: application/grpc)
        let mut header_list: Vec<(&[u8], &[u8], bool)> = Vec::with_capacity(headers.len() + 2);
        header_list.push((b":status", b"200", false));
        header_list.push((b"content-type", b"application/grpc+proto", false));

        for &(name, value) in headers {
            header_list.push((name, value, false));
        }

        let header_block = self
            .hpack_encoder
            .encode(&header_list)
            .map_err(|e| Http2Error::HpackEncode(e.to_string()))?;

        // HEADERS with end_stream=false (body + trailers follow)。連結バッファへ積み、
        // フラッシュせず続く DATA と 1 回の書き込みにまとめる（送信ホットパス最適化）。
        self.frame_encoder.encode_headers_into(
            &mut self.write_buf,
            stream_id,
            &header_block,
            false, // end_stream
            true,  // end_headers
            None,
        );

        // ストリーム状態を更新
        if let Some(stream) = self.streams.get(stream_id) {
            stream.send_headers(false)?;
        }

        // 2. DATA フレーム送信（send_data が末尾でまとめてフラッシュする）
        if let Some(body) = body {
            if !body.is_empty() {
                self.send_data(stream_id, body, false).await?;
            } else {
                // 空ボディ: HEADERS を先に送出しておく（トレイラーは別途送出）。
                self.flush_write_buf().await?;
            }
        } else {
            self.flush_write_buf().await?;
        }

        // 3. TRAILERS フレーム送信 (grpc-status, grpc-message)
        self.send_grpc_trailers(stream_id, grpc_status, grpc_message)
            .await
    }

    /// gRPC トレイラーを送信
    ///
    /// grpc-status と grpc-message をトレイラーとして送信し、
    /// ストリームを END_STREAM でクローズします。
    #[cfg(feature = "grpc")]
    pub async fn send_grpc_trailers(
        &mut self,
        stream_id: u32,
        grpc_status: u32,
        grpc_message: Option<&str>,
    ) -> Http2Result<()> {
        use crate::grpc::status::{GrpcStatus, GrpcStatusCode};

        let code = GrpcStatusCode::from_u8(grpc_status as u8).unwrap_or(GrpcStatusCode::Unknown);

        let status = if let Some(msg) = grpc_message {
            GrpcStatus::error(code, msg)
        } else {
            GrpcStatus::from_code(code)
        };
        let trailers = status.to_trailers();

        // トレイラーをエンコード
        let trailer_list: Vec<(&[u8], &[u8], bool)> = trailers
            .iter()
            .map(|(n, v)| (n.as_slice(), v.as_slice(), false))
            .collect();

        let trailer_block = self
            .hpack_encoder
            .encode(&trailer_list)
            .map_err(|e| Http2Error::HpackEncode(e.to_string()))?;

        // HEADERS (as trailers) with end_stream=true
        let trailer_frame = self.frame_encoder.encode_headers(
            stream_id,
            &trailer_block,
            true, // end_stream
            true, // end_headers
            None,
        );
        self.write_all(trailer_frame).await?;

        // ストリーム状態を更新
        if let Some(stream) = self.streams.get(stream_id) {
            stream.send_headers(true)?;
            if let Some(grpc_state) = stream.grpc_state_mut() {
                grpc_state.on_trailers_sent();
            }
        }

        Ok(())
    }
    ///
    /// フロー制御ウィンドウを考慮してデータを分割送信します。
    /// ウィンドウが不足した場合は WINDOW_UPDATE を待機します。
    pub async fn send_data(
        &mut self,
        stream_id: u32,
        data: &[u8],
        end_stream: bool,
    ) -> Http2Result<()> {
        let max_frame_size = self.remote_settings.max_frame_size as usize;
        let mut offset = 0;
        let mut window_update_wait_count = 0;
        const MAX_WINDOW_UPDATE_WAITS: usize = 100; // 無限ループ防止

        while offset < data.len() {
            // 送信可能な最大サイズを計算（フレームサイズとウィンドウの両方を考慮）
            let remaining = data.len() - offset;

            // ストリームウィンドウを取得
            let stream_window = self
                .streams
                .get_ref(stream_id)
                .map(|s| s.send_window)
                .unwrap_or(0);

            // コネクションとストリームの両方のウィンドウを考慮
            let available_window = self.conn_send_window.min(stream_window).max(0) as usize;

            if available_window == 0 {
                // ウィンドウが0の場合、WINDOW_UPDATEを待つ。ブロックする前に、連結バッファに
                // 溜めた DATA を必ず送出しておく（順序保証 + 相手のウィンドウ回復を促す）。
                self.flush_write_buf().await?;

                window_update_wait_count += 1;
                if window_update_wait_count > MAX_WINDOW_UPDATE_WAITS {
                    return Err(Http2Error::stream_error(
                        stream_id,
                        Http2ErrorCode::FlowControlError,
                        "Flow control window exhausted after max waits",
                    ));
                }

                // WINDOW_UPDATE フレームを読み込む
                match self.read_frame().await {
                    Ok(frame) => {
                        // WINDOW_UPDATE の処理
                        if let Frame::WindowUpdate {
                            stream_id: wid,
                            increment,
                        } = frame
                        {
                            self.handle_window_update(wid, increment)?;
                        } else {
                            // 他のフレームも処理（PING、SETTINGS など）
                            let _ = self.process_frame(frame).await;
                        }
                    }
                    Err(Http2Error::ConnectionClosed) => {
                        return Err(Http2Error::ConnectionClosed);
                    }
                    Err(e) => {
                        // 読み取りエラーの場合は続行を試みる
                        if !matches!(e, Http2Error::Io(ref io_err) if io_err.kind() == io::ErrorKind::WouldBlock)
                        {
                            return Err(e);
                        }
                    }
                }
                continue;
            }

            // 送信可能なチャンクサイズを決定
            let chunk_len = remaining.min(max_frame_size).min(available_window);
            let is_last = offset + chunk_len >= data.len();
            let chunk = &data[offset..offset + chunk_len];
            let len = chunk.len() as i32;

            // ウィンドウを減少
            self.conn_send_window -= len;
            if let Some(stream) = self.streams.get(stream_id) {
                stream.send_window -= len;
            }

            // DATA フレームを連結バッファへ追記（per-frame Vec 確保を排除）。HEADERS と
            // 同一バッファに載せることで 1 レスポンスの書き込みを 1 回にまとめる。
            self.frame_encoder.encode_data_into(
                &mut self.write_buf,
                stream_id,
                chunk,
                end_stream && is_last,
            );

            // 連結バッファが閾値を超えたら途中フラッシュしてメモリを抑え、送信をパイプライン化。
            if self.write_buf.len() >= WRITE_BUF_FLUSH_THRESHOLD {
                self.flush_write_buf().await?;
            }

            offset += chunk_len;
            window_update_wait_count = 0; // 送信成功したのでリセット
        }

        // 空ボディ + END_STREAM: 上のループは data.len()==0 で回らないため、ここで
        // 0 長 DATA フレームを明示送出して END_STREAM を伝える。ストリーミング転送
        // （F-32）でバックエンドが content-length 未達で切断した際などの終端クローズに使う。
        if data.is_empty() && end_stream {
            self.frame_encoder
                .encode_data_into(&mut self.write_buf, stream_id, &[], true);
        }

        // 連結バッファに残ったフレーム（HEADERS + 全 DATA）を 1 回で送出する。
        self.flush_write_buf().await?;

        // 状態更新
        if end_stream {
            if let Some(stream) = self.streams.get(stream_id) {
                stream.send_end_stream()?;
            }
        }

        Ok(())
    }

    /// GOAWAY を送信
    pub async fn send_goaway(
        &mut self,
        error_code: Http2ErrorCode,
        debug_data: &[u8],
    ) -> Http2Result<()> {
        if self.goaway_sent {
            return Ok(());
        }

        let last_stream_id = self.streams.max_client_stream_id();
        let frame = self
            .frame_encoder
            .encode_goaway(last_stream_id, error_code as u32, debug_data);
        self.write_all(frame).await?;
        self.goaway_sent = true;

        Ok(())
    }

    /// RST_STREAM を送信
    pub async fn send_rst_stream(
        &mut self,
        stream_id: u32,
        error_code: Http2ErrorCode,
    ) -> Http2Result<()> {
        let frame = self
            .frame_encoder
            .encode_rst_stream(stream_id, error_code as u32);
        self.write_all(frame).await?;

        if let Some(stream) = self.streams.get(stream_id) {
            stream.send_rst_stream();
        }

        Ok(())
    }

    /// アイドルタイムアウトを超過したストリームをクリーンアップ (Slow Loris 対策)
    ///
    /// リクエストが完了しないストリームを検出し、RST_STREAM(CANCEL) で閉じる。
    /// この関数は定期的に呼び出す必要がある（例: 10秒ごと）。
    ///
    /// 戻り値: クリーンアップしたストリーム数
    pub async fn cleanup_idle_streams(&mut self) -> Http2Result<usize> {
        let timeout_secs = self.local_settings.stream_idle_timeout_secs;

        // タイムアウトが0の場合は無効
        if timeout_secs == 0 {
            return Ok(0);
        }

        let idle_streams = self.streams.get_idle_streams(timeout_secs);
        let count = idle_streams.len();

        for stream_id in idle_streams {
            ftlog::debug!(
                "[HTTP/2] Closing idle stream {} (timeout: {}s)",
                stream_id,
                timeout_secs
            );
            self.send_rst_stream(stream_id, Http2ErrorCode::Cancel)
                .await?;
        }

        if count > 0 {
            ftlog::info!(
                "[HTTP/2] Cleaned up {} idle streams (timeout: {}s)",
                count,
                timeout_secs
            );
        }

        Ok(count)
    }

    /// メインループ: フレームを読み込んで処理
    ///
    /// 各リクエストに対してデフォルトのレスポンス（200 OK）を返します。
    pub async fn run_simple(&mut self) -> Http2Result<()> {
        loop {
            // GOAWAY 受信後は新しいストリームを受け付けない
            if self.goaway_received {
                break;
            }

            // フレームを読み込み
            let frame = match self.read_frame().await {
                Ok(f) => f,
                Err(Http2Error::ConnectionClosed) => break,
                Err(Http2Error::Io(e)) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => {
                    // エラー時は GOAWAY を送信
                    let _ = self
                        .send_goaway(e.error_code(), e.to_string().as_bytes())
                        .await;
                    return Err(e);
                }
            };

            // フレームを処理
            match self.process_frame(frame).await {
                Ok(Some(req)) if !req.body_pending => {
                    // リクエストが完了 - デフォルトレスポンスを送信
                    let headers: &[(&[u8], &[u8])] =
                        &[(b"content-type", b"text/plain"), (b"server", b"veil/http2")];
                    if let Err(e) = self
                        .send_response(req.stream_id, 200, headers, Some(b"HTTP/2 OK"))
                        .await
                    {
                        // ストリームエラーの場合は RST_STREAM を送信
                        if let Some(id) = e.rst_stream_id() {
                            let _ = self.send_rst_stream(id, e.error_code()).await;
                        } else if e.should_goaway() {
                            let _ = self
                                .send_goaway(e.error_code(), e.to_string().as_bytes())
                                .await;
                            return Err(e);
                        }
                    }
                }
                Ok(Some(_)) => {
                    // body_pending: ヘッダーのみ完了。ボディ受信を待つ（簡易サーバーは
                    // ストリーミングしないため END_STREAM まで蓄積させる）。
                }
                Ok(None) => {
                    // フレーム処理完了、次のフレームへ
                }
                Err(e) => {
                    if e.should_goaway() {
                        let _ = self
                            .send_goaway(e.error_code(), e.to_string().as_bytes())
                            .await;
                        return Err(e);
                    } else if let Some(id) = e.rst_stream_id() {
                        let _ = self.send_rst_stream(id, e.error_code()).await;
                    }
                }
            }

            // クリーンアップ
            self.streams.cleanup_closed();
        }

        Ok(())
    }

    /// ストリームを取得
    pub fn get_stream(&self, stream_id: u32) -> Option<&Stream> {
        self.streams.get_ref(stream_id)
    }

    /// ストリームを可変で取得
    pub fn get_stream_mut(&mut self, stream_id: u32) -> Option<&mut Stream> {
        self.streams.get(stream_id)
    }

    /// 基盤ストリームへの参照を取得
    pub fn get_inner(&self) -> &S {
        &self.stream
    }

    /// クローズ済みストリームをクリーンアップ（外部からアクセス可能）
    pub fn cleanup_closed(&mut self) {
        self.streams.cleanup_closed();
    }

    /// DATA フレームを送信ウィンドウの許す範囲だけ連結バッファへ積む（await しない、F-116）。
    ///
    /// `min(コネクション送信ウィンドウ, ストリーム送信ウィンドウ)` の範囲を
    /// `max_frame_size` で分割して `write_buf` へ追記し、両ウィンドウを減算して
    /// **積めたバイト数を返す**。I/O（読み書き）は一切行わず、実際の送出はメインループが
    /// [`flush_write_buf`](Self::flush_write_buf) で明示的に行う。
    ///
    /// - END_STREAM は「`data` の残り全量を積み切った」場合のみ、最終バイトを運ぶフレームに
    ///   付与する。ウィンドウ不足で途中までしか積めなかった場合は付与せず、呼び出し側が
    ///   WINDOW_UPDATE 受信後に残りを再度渡す（END_STREAM 遅延）。
    /// - `data` が空で `end_stream=true` の場合は 0 長の END_STREAM DATA フレームを積む
    ///   （空ボディの終端クローズ用）。
    ///
    /// 既存の [`send_data`](Self::send_data)（ウィンドウ枯渇時に内部で `read_frame` する
    /// ブロッキング版）はクライアント/非多重化経路用に無改変で残す。
    pub fn queue_data_frames(
        &mut self,
        stream_id: u32,
        data: &[u8],
        end_stream: bool,
    ) -> Http2Result<usize> {
        // 空ボディ + END_STREAM: 0 長 DATA フレームで終端を伝える（send_data と同挙動）。
        if data.is_empty() {
            if end_stream {
                self.frame_encoder
                    .encode_data_into(&mut self.write_buf, stream_id, &[], true);
                if let Some(stream) = self.streams.get(stream_id) {
                    stream.send_end_stream()?;
                }
            }
            return Ok(0);
        }

        let max_frame_size = self.remote_settings.max_frame_size as usize;
        let mut offset = 0;

        while offset < data.len() {
            // ストリームウィンドウを取得
            let stream_window = self
                .streams
                .get_ref(stream_id)
                .map(|s| s.send_window)
                .unwrap_or(0);

            // コネクションとストリームの両方のウィンドウを考慮
            let available_window = self.conn_send_window.min(stream_window).max(0) as usize;
            if available_window == 0 {
                // ウィンドウ枯渇: 待たずに「ここまで積めた」を返す（非ブロッキング）。
                break;
            }

            let remaining = data.len() - offset;
            let chunk_len = remaining.min(max_frame_size).min(available_window);
            let is_last = offset + chunk_len >= data.len();
            let chunk = &data[offset..offset + chunk_len];
            let len = chunk_len as i32;

            // ウィンドウを減少
            self.conn_send_window -= len;
            if let Some(stream) = self.streams.get(stream_id) {
                stream.send_window -= len;
            }

            // DATA フレームを連結バッファへ追記（per-frame Vec 確保を排除）。
            // END_STREAM は全量積み切りが確定した最終フレームのみに付与。
            self.frame_encoder.encode_data_into(
                &mut self.write_buf,
                stream_id,
                chunk,
                end_stream && is_last,
            );

            offset += chunk_len;
        }

        // 全量積み切って END_STREAM を付与した場合のみ状態遷移。
        if end_stream && offset == data.len() {
            if let Some(stream) = self.streams.get(stream_id) {
                stream.send_end_stream()?;
            }
        }

        Ok(offset)
    }

    /// ストリームからリクエスト情報を所有権ごと取り出す（F-116、コピーなし）。
    ///
    /// per-stream リクエストタスクへ conn 非依存でリクエストを引き渡すため、
    /// `Stream` に蓄積された疑似ヘッダー・通常ヘッダー・ボディを `std::mem::take` で
    /// 移動する（クローンしない）。ストリーム自体は `StreamManager` に残り、
    /// 状態遷移・フロー制御・`cleanup_closed` は従来どおり機能する。
    ///
    /// ストリームが存在しない場合は `None`。`:scheme` 等の抽出対象外の疑似ヘッダーは
    /// `headers` にそのまま残る（呼び出し側は従来どおり `:` プレフィックスで除外できる）。
    pub fn take_request_parts(&mut self, stream_id: u32) -> Option<H2RequestParts> {
        let stream = self.streams.get(stream_id)?;
        let all_headers = std::mem::take(&mut stream.request_headers);
        let body = std::mem::take(&mut stream.request_body);

        let mut method = Vec::new();
        let mut path = Vec::new();
        let mut authority = None;
        let mut headers = Vec::with_capacity(all_headers.len());
        for h in all_headers {
            match h.name.as_slice() {
                b":method" => method = h.value,
                b":path" => path = h.value,
                b":authority" => authority = Some(h.value),
                _ => headers.push(h),
            }
        }

        Some(H2RequestParts {
            method,
            path,
            authority,
            headers,
            body,
        })
    }
}

impl<S: std::os::fd::AsRawFd> Http2Connection<S> {
    /// 基盤ストリームの生ファイルディスクリプタを取得する（F-116）。
    ///
    /// HTTP/2 多重化メインループが `wait_readable_fd`（`POLL_ADD`）でソケット可読を
    /// 待機するために使う（`ReadFuture` の drop は既読データを破棄するため、
    /// `read` と notify の `select` は禁止。データ転送を伴わない POLL_ADD で待つ）。
    pub fn raw_fd(&self) -> std::os::fd::RawFd {
        self.stream.as_raw_fd()
    }
}

impl<S: crate::runtime::io::BufferedReadState> Http2Connection<S> {
    /// 「POLLIN では通知されない、既に手元にある未消費入力」が残っているか（F-116）。
    ///
    /// 読み込みバッファ（`read_buf[buf_start..buf_end]`）の未消化バイト、または基盤
    /// ストリームの復号済み/先読みバッファ
    /// （[`BufferedReadState::has_buffered_read_data`]）のいずれかが残っていれば `true`。
    /// メインループはこれが `false` のときのみ `wait_readable_fd` で待機してよい
    /// （さもないと既読データが残ったまま POLLIN を待ってデッドロックする）。
    pub fn has_pending_input(&self) -> bool {
        self.buf_end > self.buf_start || self.stream.has_buffered_read_data()
    }

    /// 基盤ストリームに復号済み/先読みバッファが残っているか（F-116）。
    ///
    /// TLS フォールバック経路では復号済み平文がストリーム内部に滞留し、POLLIN では
    /// 通知されない。メインループはこれが `true` の間 `wait_readable_fd` を当てにできず、
    /// `fill_read_buf` で能動的に読み出す必要がある（`read_buf` の未消化バイト＝分割フレーム
    /// 待ちとは区別する。後者は POLLIN が発火するため通常の待機で拾える）。
    pub fn has_stream_buffered_read_data(&self) -> bool {
        self.stream.has_buffered_read_data()
    }
}

/// 処理済みリクエスト
#[derive(Debug)]
pub struct ProcessedRequest {
    /// ストリーム ID
    pub stream_id: u32,
    /// ヘッダーは完了したがボディ（DATA フレーム）が継続中かどうか。
    ///
    /// `true` の場合、HEADERS（+ CONTINUATION）が完了し疑似ヘッダーがデコード済みだが、
    /// END_STREAM はまだ受信していない（リクエストボディが後続する）ことを示す。
    /// F-32 リクエスト方向ストリーミングの起点で、呼び出し側はこの時点でバックエンド
    /// 接続を開始し DATA フレームを逐次転送できる。ストリーミング非適格なら呼び出し側は
    /// 無視してよく、その場合 DATA は従来どおり `Stream::request_body` に蓄積され、
    /// END_STREAM 受信時に `body_pending: false` の `ProcessedRequest` が返る。
    pub body_pending: bool,
}

/// ストリームから所有権ごと取り出したリクエスト情報（F-116）。
///
/// [`Http2Connection::take_request_parts`] が `Stream` の蓄積状態から `std::mem::take` で
/// 移動して構築する（クローンなし）。per-stream リクエストタスクは本構造体だけを受け取り、
/// `Http2Connection` に触れずにルーティング・バックエンド往復を実行できる。
#[derive(Debug)]
pub struct H2RequestParts {
    /// `:method` 疑似ヘッダーの値。
    pub method: Vec<u8>,
    /// `:path` 疑似ヘッダーの値。
    pub path: Vec<u8>,
    /// `:authority` 疑似ヘッダーの値（無ければ `None`。host ヘッダーへの
    /// フォールバックは呼び出し側で行う）。
    pub authority: Option<Vec<u8>>,
    /// 抽出済み疑似ヘッダー（`:method`/`:path`/`:authority`）以外のヘッダー一覧
    /// （`:scheme` 等の残余疑似ヘッダーを含む）。
    pub headers: Vec<crate::http2::hpack::HeaderField>,
    /// 蓄積済みリクエストボディ（`Stream::request_body` からの移動。
    /// `freeze()` でゼロコピーに `Bytes` 化できる）。
    pub body: bytes::BytesMut,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ====================
    // CONNECTION_PREFACE テスト
    // ====================

    #[test]
    fn test_connection_preface_value() {
        // HTTP/2 コネクションプリフェースの正確な値を検証
        assert_eq!(CONNECTION_PREFACE, b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n");
    }

    #[test]
    fn test_connection_preface_length() {
        // プリフェースの長さは24バイト
        assert_eq!(CONNECTION_PREFACE.len(), 24);
    }

    // ====================
    // ProcessedRequest テスト
    // ====================

    #[test]
    fn test_processed_request_creation() {
        // ProcessedRequestの作成
        let req = ProcessedRequest {
            stream_id: 1,
            body_pending: false,
        };
        assert_eq!(req.stream_id, 1);
        assert!(!req.body_pending);

        // ヘッダー完了・ボディ継続（ストリーミング起点）
        let req2 = ProcessedRequest {
            stream_id: 3,
            body_pending: true,
        };
        assert_eq!(req2.stream_id, 3);
        assert!(req2.body_pending);
    }

    #[test]
    fn test_processed_request_odd_stream_ids() {
        // クライアント開始ストリームは奇数ID
        let req = ProcessedRequest {
            stream_id: 1,
            body_pending: false,
        };
        assert!(req.stream_id % 2 == 1);

        let req2 = ProcessedRequest {
            stream_id: 5,
            body_pending: false,
        };
        assert!(req2.stream_id % 2 == 1);
    }

    // ====================
    // Http2Settings 統合テスト
    // ====================

    #[test]
    fn test_default_settings() {
        // デフォルト設定の検証
        let settings = Http2Settings::default();

        // RFC 7540 デフォルト値
        assert!(settings.max_concurrent_streams > 0);
        assert!(settings.initial_window_size > 0);
        assert!(settings.max_frame_size >= 16384); // 最小値
        assert!(settings.max_frame_size <= 16777215); // 最大値
    }

    #[test]
    fn test_settings_encode_decode() {
        // 設定のエンコード
        let settings = Http2Settings::default();
        let encoded = settings.encode();

        // エンコード結果は6の倍数（各設定は6バイト: ID 2バイト + 値 4バイト）
        assert!(encoded.len() % 6 == 0);
    }

    // ====================
    // フレームサイズ制約テスト
    // ====================

    #[test]
    fn test_frame_size_constraints() {
        // RFC 7540 Section 4.2: フレームサイズ制約
        let min_frame_size = 16384u32; // 2^14
        let max_frame_size = 16777215u32; // 2^24 - 1

        let settings = Http2Settings::default();

        assert!(settings.max_frame_size >= min_frame_size);
        assert!(settings.max_frame_size <= max_frame_size);
    }

    // ====================
    // ウィンドウサイズテスト
    // ====================

    #[test]
    fn test_window_size_constraints() {
        // RFC 7540 Section 6.9.2: ウィンドウサイズ制約
        let max_window_size = 2147483647i32; // 2^31 - 1

        let settings = Http2Settings::default();

        assert!(settings.initial_window_size > 0);
        assert!((settings.initial_window_size as i32) <= max_window_size);
    }

    // ====================
    // 送信ホットパス連結（coalescing）テスト
    // ====================
    //
    // モックストリームで `write` 呼び出し回数と送出バイト列を捕捉し、送信ホットパスの
    // フレーム連結（HEADERS + DATA を 1 回の書き込みにまとめる最適化・F-73 続き）が
    // 実際に機能していること、およびフレーム内容が RFC 準拠であることを検証する。

    use crate::runtime::buf::IoBuf;
    use crate::runtime::io::{AsyncReadRent, AsyncWriteRent, BufResult};
    use std::future::Future;

    /// 送出バイト列と write 呼び出し回数を記録するモックストリーム。
    ///
    /// - `write`: 呼び出しごとに渡されたバイト列を 1 要素として `writes` に記録する
    ///   （= 何回の書き込みに分割されたかを厳密に観測できる）。常に全長書き込み成功を返す。
    /// - `read`: 送信専用テストでは呼ばれない前提（ウィンドウ枯渇待ちに入ると呼ばれるが、
    ///   本テストは十分なウィンドウ内でのみ送信する）。呼ばれた場合は EOF(0) を返す。
    struct RecordingStream {
        writes: Vec<Vec<u8>>,
    }

    impl RecordingStream {
        fn new() -> Self {
            Self { writes: Vec::new() }
        }

        /// 記録された全書き込みを連結したバイト列。
        fn concat(&self) -> Vec<u8> {
            self.writes.iter().flatten().copied().collect()
        }
    }

    impl AsyncReadRent for RecordingStream {
        async fn read<T: crate::runtime::buf::IoBufMut>(&mut self, buf: T) -> BufResult<usize, T> {
            // 送信専用テストでは読み取りは EOF 扱い（ウィンドウ枯渇待ちに入らせない）。
            (Ok(0), buf)
        }
    }

    impl crate::runtime::io::BufferedReadState for RecordingStream {
        // モックは復号済み/先読みバッファを持たない（has_pending_input テスト用）。
        fn has_buffered_read_data(&self) -> bool {
            false
        }
    }

    impl AsyncWriteRent for RecordingStream {
        async fn write<T: IoBuf>(&mut self, buf: T) -> BufResult<usize, T> {
            let len = buf.bytes_init();
            // SAFETY: read_ptr()..len は IoBuf の初期化済み領域。write 完了までバッファは生存。
            let slice = unsafe { std::slice::from_raw_parts(buf.read_ptr(), len) };
            self.writes.push(slice.to_vec());
            (Ok(len), buf)
        }

        async fn shutdown(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// 単純な同期ドライバ（io_uring 不要な送信テスト用）。Pending は自己 wake 前提で即再試行。
    fn drive<F: Future>(mut fut: F) -> F::Output {
        use std::sync::Arc;
        use std::task::{Context, Poll, Wake, Waker};
        struct NoopWake;
        impl Wake for NoopWake {
            fn wake(self: Arc<Self>) {}
        }
        let waker = Waker::from(Arc::new(NoopWake));
        let mut cx = Context::from_waker(&waker);
        let mut fut = unsafe { std::pin::Pin::new_unchecked(&mut fut) };
        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(v) => return v,
                Poll::Pending => {}
            }
        }
    }

    /// 連結バイト列を HTTP/2 フレーム列 (type, flags, stream_id, payload) にパースする。
    fn parse_frames(bytes: &[u8]) -> Vec<(u8, u8, u32, Vec<u8>)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 9 <= bytes.len() {
            let len = ((bytes[i] as usize) << 16)
                | ((bytes[i + 1] as usize) << 8)
                | bytes[i + 2] as usize;
            let ftype = bytes[i + 3];
            let flags = bytes[i + 4];
            let sid = u32::from_be_bytes([
                bytes[i + 5] & 0x7f,
                bytes[i + 6],
                bytes[i + 7],
                bytes[i + 8],
            ]);
            assert!(i + 9 + len <= bytes.len(), "truncated frame payload");
            let payload = bytes[i + 9..i + 9 + len].to_vec();
            out.push((ftype, flags, sid, payload));
            i += 9 + len;
        }
        assert_eq!(i, bytes.len(), "trailing bytes not frame-aligned");
        out
    }

    const FRAME_DATA: u8 = 0x0;
    const FRAME_HEADERS: u8 = 0x1;
    const FLAG_END_STREAM: u8 = 0x1;
    const FLAG_END_HEADERS: u8 = 0x4;

    /// HalfClosedRemote（= リクエスト受信済み）状態のストリームを 1 本持つ接続を用意する。
    fn conn_with_open_stream(stream_id: u32) -> Http2Connection<RecordingStream> {
        let mut conn = Http2Connection::new(RecordingStream::new(), Http2Settings::default());
        let stream = conn
            .streams
            .get_or_create_client_stream(stream_id)
            .expect("create stream");
        // リクエストを END_STREAM 付きで受信した想定（ボディ無しリクエスト）。
        stream.recv_headers(true).expect("recv_headers");
        conn
    }

    #[test]
    fn send_response_coalesces_headers_and_data_into_one_write() {
        let mut conn = conn_with_open_stream(1);
        let body = b"hello world";
        drive(conn.send_response(1, 200, &[(b"content-type", b"text/plain")], Some(body)))
            .expect("send_response");

        // HEADERS + DATA が 1 回の write に連結されていること（システムコール削減の核心）。
        assert_eq!(
            conn.stream.writes.len(),
            1,
            "HEADERS と DATA は 1 回の書き込みに連結されるべき"
        );

        let frames = parse_frames(&conn.stream.concat());
        assert_eq!(frames.len(), 2, "HEADERS + DATA の 2 フレーム");

        // 1 フレーム目: HEADERS（END_HEADERS 有・END_STREAM 無）
        assert_eq!(frames[0].0, FRAME_HEADERS);
        assert_eq!(frames[0].1 & FLAG_END_HEADERS, FLAG_END_HEADERS);
        assert_eq!(frames[0].1 & FLAG_END_STREAM, 0);
        assert_eq!(frames[0].2, 1);

        // 2 フレーム目: DATA（END_STREAM 有・ボディ一致）
        assert_eq!(frames[1].0, FRAME_DATA);
        assert_eq!(frames[1].1 & FLAG_END_STREAM, FLAG_END_STREAM);
        assert_eq!(frames[1].2, 1);
        assert_eq!(frames[1].3, body);
    }

    #[test]
    fn send_response_empty_body_is_single_headers_frame() {
        let mut conn = conn_with_open_stream(1);
        drive(conn.send_response(1, 204, &[(b"x-test", b"1")], None)).expect("send_response");

        assert_eq!(conn.stream.writes.len(), 1, "ヘッダのみで 1 write");
        let frames = parse_frames(&conn.stream.concat());
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, FRAME_HEADERS);
        // ボディ無しは HEADERS に END_STREAM が立つ。
        assert_eq!(frames[0].1 & FLAG_END_STREAM, FLAG_END_STREAM);
        assert_eq!(frames[0].1 & FLAG_END_HEADERS, FLAG_END_HEADERS);
    }

    #[test]
    fn send_headers_buffered_then_data_coalesces_into_one_write() {
        // ストリーミング応答経路の連結（send_headers_buffered → send_data）を検証。
        let mut conn = conn_with_open_stream(1);
        let body = b"streamed-body-chunk";
        drive(async {
            conn.send_headers_buffered(1, 200, &[(b"content-type", b"application/octet-stream")])
                .await?;
            conn.send_data(1, body, true).await
        })
        .expect("streaming send");

        assert_eq!(
            conn.stream.writes.len(),
            1,
            "buffered HEADERS と続く DATA は 1 write に連結されるべき"
        );
        let frames = parse_frames(&conn.stream.concat());
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].0, FRAME_HEADERS);
        assert_eq!(frames[0].1 & FLAG_END_STREAM, 0);
        assert_eq!(frames[1].0, FRAME_DATA);
        assert_eq!(frames[1].1 & FLAG_END_STREAM, FLAG_END_STREAM);
        assert_eq!(frames[1].3, body);
    }

    #[test]
    fn send_data_splits_by_max_frame_size_but_still_one_write() {
        // remote の max_frame_size を小さくし、ボディが複数 DATA フレームに分割されても
        // ウィンドウ内なら 1 write に連結され、ボディ整合性と END_STREAM 位置が正しいこと。
        let mut conn = conn_with_open_stream(1);
        conn.remote_settings.max_frame_size = 4; // 4 バイトごとに分割
        let body = b"0123456789"; // 10 バイト → 4,4,2 の 3 フレーム
        drive(async {
            conn.send_headers_buffered(1, 200, &[(b"content-type", b"text/plain")])
                .await?;
            conn.send_data(1, body, true).await
        })
        .expect("send");

        assert_eq!(conn.stream.writes.len(), 1, "閾値未満なので 1 write に連結");
        let frames = parse_frames(&conn.stream.concat());
        // HEADERS + 3 DATA
        assert_eq!(frames.len(), 4);
        assert_eq!(frames[0].0, FRAME_HEADERS);

        let data_frames: Vec<_> = frames[1..].iter().collect();
        assert_eq!(data_frames.len(), 3);
        // 各 DATA は max_frame_size 以下。
        for f in &data_frames {
            assert_eq!(f.0, FRAME_DATA);
            assert!(f.3.len() <= 4);
        }
        // END_STREAM は最後の DATA にのみ立つ。
        assert_eq!(data_frames[0].1 & FLAG_END_STREAM, 0);
        assert_eq!(data_frames[1].1 & FLAG_END_STREAM, 0);
        assert_eq!(data_frames[2].1 & FLAG_END_STREAM, FLAG_END_STREAM);
        // ボディ全体を連結すると元データに一致（ゼロコピー分割の整合性）。
        let reassembled: Vec<u8> = data_frames.iter().flat_map(|f| f.3.clone()).collect();
        assert_eq!(reassembled, body);
    }

    #[test]
    fn write_buf_is_returned_to_pool_and_reused() {
        // 送信後に write_buf が空へ戻り（呼び出し境界の不変条件）、容量が再利用可能なこと。
        let mut conn = conn_with_open_stream(1);
        drive(conn.send_response(1, 200, &[(b"a", b"b")], Some(b"body"))).expect("send");
        assert!(
            conn.write_buf.is_empty(),
            "呼び出し境界では write_buf は空であるべき"
        );
        assert!(
            conn.write_buf.capacity() > 0,
            "書き込み後にバッファ容量が再利用のため保持されるべき"
        );
    }

    // ====================
    // F-116 Stage 2: 同期フレームパース / 非ブロッキング DATA 送出 / リクエスト取り出し
    // ====================

    const FRAME_PING: u8 = 0x6;

    /// 読み込みバッファ末尾へバイト列を直接追記する（I/O なしでの受信をシミュレート）。
    fn feed_read_buf(conn: &mut Http2Connection<RecordingStream>, data: &[u8]) {
        conn.read_buf[conn.buf_end..conn.buf_end + data.len()].copy_from_slice(data);
        conn.buf_end += data.len();
    }

    #[test]
    fn try_read_frame_buffered_partial_then_complete() {
        // 部分フレーム → Ok(None)、残りのバイトが揃うと Ok(Some) になること。
        let ping = FrameEncoder::new(16384).encode_ping(&[0xAB; 8], false);
        // 先頭 5 バイト（ヘッダー 9 バイトにも満たない）だけ投入。
        let mut conn = Http2Connection::new_with_initial_buffer(
            RecordingStream::new(),
            Http2Settings::default(),
            ping[..5].to_vec(),
        );
        assert!(
            matches!(conn.try_read_frame_buffered(), Ok(None)),
            "ヘッダー未達は Ok(None)"
        );

        // ヘッダーは揃うがペイロードが未達。
        feed_read_buf(&mut conn, &ping[5..12]);
        assert!(
            matches!(conn.try_read_frame_buffered(), Ok(None)),
            "ペイロード未達は Ok(None)"
        );

        // 全量揃うとフレームが返る。
        feed_read_buf(&mut conn, &ping[12..]);
        let frame = conn
            .try_read_frame_buffered()
            .expect("parse")
            .expect("complete frame");
        assert!(matches!(frame, Frame::Ping { ack: false, data } if data == [0xAB; 8]));
        // 消費済み: 次は Ok(None)。
        assert!(matches!(conn.try_read_frame_buffered(), Ok(None)));
        // RecordingStream の read は一度も呼ばれていない（同期パースのみ）。
        assert!(conn.stream.writes.is_empty());
    }

    #[test]
    fn try_read_frame_buffered_multiple_frames_without_io() {
        // 1 バッファに載った複数フレームを I/O なしで連続パースできること。
        let encoder = FrameEncoder::new(16384);
        let mut bytes = encoder.encode_ping(&[1u8; 8], false);
        bytes.extend_from_slice(&encoder.encode_ping(&[2u8; 8], true));
        bytes.extend_from_slice(&encoder.encode_window_update(0, 1000));

        let mut conn = Http2Connection::new_with_initial_buffer(
            RecordingStream::new(),
            Http2Settings::default(),
            bytes,
        );
        let f1 = conn.try_read_frame_buffered().unwrap().expect("frame 1");
        assert!(matches!(f1, Frame::Ping { ack: false, data } if data == [1u8; 8]));
        let f2 = conn.try_read_frame_buffered().unwrap().expect("frame 2");
        assert!(matches!(f2, Frame::Ping { ack: true, data } if data == [2u8; 8]));
        let f3 = conn.try_read_frame_buffered().unwrap().expect("frame 3");
        assert!(matches!(
            f3,
            Frame::WindowUpdate {
                stream_id: 0,
                increment: 1000
            }
        ));
        assert!(matches!(conn.try_read_frame_buffered(), Ok(None)));
    }

    #[test]
    fn queue_data_frames_clamps_to_connection_window() {
        // コネクションウィンドウ < ストリームウィンドウのとき、conn 側で制限されること。
        let mut conn = conn_with_open_stream(1);
        conn.conn_send_window = 6; // ストリームは 65535
        let data = b"0123456789";
        let queued = conn.queue_data_frames(1, data, true).expect("queue");
        assert_eq!(queued, 6, "conn ウィンドウ分だけ積まれる");
        assert_eq!(conn.conn_send_window, 0);
        assert_eq!(conn.streams.get_ref(1).unwrap().send_window, 65535 - 6);

        let frames = parse_frames(&conn.write_buf);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, FRAME_DATA);
        assert_eq!(frames[0].3, b"012345");
        // 全量積み切れていないので END_STREAM は付かない（遅延）。
        assert_eq!(frames[0].1 & FLAG_END_STREAM, 0);
    }

    #[test]
    fn queue_data_frames_clamps_to_stream_window() {
        // ストリームウィンドウ < コネクションウィンドウのとき、ストリーム側で制限されること。
        let mut conn = conn_with_open_stream(1);
        conn.streams.get(1).unwrap().send_window = 4;
        let data = b"0123456789";
        let queued = conn.queue_data_frames(1, data, true).expect("queue");
        assert_eq!(queued, 4, "ストリームウィンドウ分だけ積まれる");
        assert_eq!(conn.streams.get_ref(1).unwrap().send_window, 0);
        assert_eq!(conn.conn_send_window, 65535 - 4);

        let frames = parse_frames(&conn.write_buf);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].3, b"0123");
        assert_eq!(frames[0].1 & FLAG_END_STREAM, 0);
    }

    #[test]
    fn queue_data_frames_splits_by_max_frame_size() {
        // max_frame_size で分割され、END_STREAM が最終フレームのみに立つこと。
        let mut conn = conn_with_open_stream(1);
        conn.remote_settings.max_frame_size = 4;
        let data = b"0123456789"; // 10 バイト → 4,4,2
        let queued = conn.queue_data_frames(1, data, true).expect("queue");
        assert_eq!(queued, 10, "ウィンドウ内なら全量積まれる");

        let frames = parse_frames(&conn.write_buf);
        assert_eq!(frames.len(), 3);
        for f in &frames {
            assert_eq!(f.0, FRAME_DATA);
            assert!(f.3.len() <= 4);
        }
        assert_eq!(frames[0].1 & FLAG_END_STREAM, 0);
        assert_eq!(frames[1].1 & FLAG_END_STREAM, 0);
        assert_eq!(frames[2].1 & FLAG_END_STREAM, FLAG_END_STREAM);
        let reassembled: Vec<u8> = frames.iter().flat_map(|f| f.3.clone()).collect();
        assert_eq!(reassembled, data);
        // 全量 + END_STREAM 送出でストリームはクローズ（HalfClosedRemote → Closed）。
        assert_eq!(conn.streams.get_ref(1).unwrap().state, StreamState::Closed);
    }

    #[test]
    fn queue_data_frames_defers_end_stream_until_fully_queued() {
        // ウィンドウ枯渇で途中まで → END_STREAM なし。回復後の残量で END_STREAM が立つこと。
        let mut conn = conn_with_open_stream(1);
        conn.conn_send_window = 4;
        let data = b"0123456789";
        let queued = conn.queue_data_frames(1, data, true).expect("queue");
        assert_eq!(queued, 4);
        {
            let frames = parse_frames(&conn.write_buf);
            assert_eq!(frames.len(), 1);
            assert_eq!(
                frames[0].1 & FLAG_END_STREAM,
                0,
                "部分送出に END_STREAM 禁止"
            );
        }
        // まだ END_STREAM を送っていないので状態は据え置き。
        assert_eq!(
            conn.streams.get_ref(1).unwrap().state,
            StreamState::HalfClosedRemote
        );

        // WINDOW_UPDATE 相当でウィンドウ回復 → 残りを積むと最終フレームに END_STREAM。
        conn.conn_send_window = 100;
        conn.write_buf.clear();
        let queued2 = conn
            .queue_data_frames(1, &data[queued..], true)
            .expect("queue rest");
        assert_eq!(queued2, 6);
        let frames = parse_frames(&conn.write_buf);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].3, b"456789");
        assert_eq!(frames[0].1 & FLAG_END_STREAM, FLAG_END_STREAM);
        assert_eq!(conn.streams.get_ref(1).unwrap().state, StreamState::Closed);
    }

    #[test]
    fn queue_data_frames_empty_body_end_stream() {
        // 空ボディ + END_STREAM は 0 長 END_STREAM DATA フレームを積むこと。
        let mut conn = conn_with_open_stream(1);
        let queued = conn.queue_data_frames(1, &[], true).expect("queue");
        assert_eq!(queued, 0);
        let frames = parse_frames(&conn.write_buf);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, FRAME_DATA);
        assert!(frames[0].3.is_empty());
        assert_eq!(frames[0].1 & FLAG_END_STREAM, FLAG_END_STREAM);
        assert_eq!(conn.streams.get_ref(1).unwrap().state, StreamState::Closed);
        // ウィンドウは消費されない。
        assert_eq!(conn.conn_send_window, 65535);
    }

    #[test]
    fn queue_data_frames_empty_body_without_end_stream_is_noop() {
        // 空ボディ + end_stream=false は何も積まない。
        let mut conn = conn_with_open_stream(1);
        let queued = conn.queue_data_frames(1, &[], false).expect("queue");
        assert_eq!(queued, 0);
        assert!(conn.write_buf.is_empty());
    }

    #[test]
    fn take_request_parts_moves_data_and_leaves_stream_reusable() {
        use crate::http2::hpack::HeaderField;

        let mut conn = conn_with_open_stream(1);
        {
            let stream = conn.streams.get(1).unwrap();
            stream.request_headers = vec![
                HeaderField::new(b":method".to_vec(), b"POST".to_vec()),
                HeaderField::new(b":scheme".to_vec(), b"https".to_vec()),
                HeaderField::new(b":path".to_vec(), b"/api/echo".to_vec()),
                HeaderField::new(b":authority".to_vec(), b"example.com".to_vec()),
                HeaderField::new(b"content-type".to_vec(), b"text/plain".to_vec()),
            ];
            stream.request_body.extend_from_slice(b"hello body");
        }

        let parts = conn.take_request_parts(1).expect("parts");
        assert_eq!(parts.method, b"POST");
        assert_eq!(parts.path, b"/api/echo");
        assert_eq!(parts.authority.as_deref(), Some(&b"example.com"[..]));
        assert_eq!(&parts.body[..], b"hello body");
        // 抽出対象外（:scheme・通常ヘッダー）は headers に残る。
        assert_eq!(parts.headers.len(), 2);
        assert!(parts.headers.iter().any(|h| h.name == b":scheme"));
        assert!(parts.headers.iter().any(|h| h.name == b"content-type"));

        // ストリーム側は空へ移動済みで、マネージャには残っている（クリーンアップ可能）。
        let stream = conn.streams.get_ref(1).unwrap();
        assert!(stream.request_headers.is_empty(), "ヘッダーは移動済み");
        assert!(stream.request_body.is_empty(), "ボディは移動済み");
        assert_eq!(stream.state, StreamState::HalfClosedRemote, "状態は不変");

        // 存在しないストリームは None。
        assert!(conn.take_request_parts(99).is_none());
    }

    #[test]
    fn has_pending_input_reflects_read_buf_and_stream_state() {
        // read_buf の未消化バイトの有無で has_pending_input が変化すること
        // （RecordingStream は BufferedReadState = false）。
        let ping = FrameEncoder::new(16384).encode_ping(&[7u8; 8], false);
        let mut conn = Http2Connection::new_with_initial_buffer(
            RecordingStream::new(),
            Http2Settings::default(),
            ping,
        );
        assert!(conn.has_pending_input(), "初期データが未消化なら true");
        let _ = conn.try_read_frame_buffered().unwrap().expect("frame");
        assert!(!conn.has_pending_input(), "全消化後は false");
    }

    #[test]
    fn recv_data_for_streaming_defers_window_update_until_replenish() {
        // F-116 バックプレッシャ: recv_data_for_streaming は WINDOW_UPDATE を送らず
        // （受信即補充だとバックエンドが遅い場合に未転送ボディが際限なく滞留する）、
        // replenish_recv_window（下流への消費連動）で初めて write_buf へ積まれること。
        const FRAME_WINDOW_UPDATE: u8 = 0x8;
        let mut conn = Http2Connection::new(RecordingStream::new(), Http2Settings::default());
        let stream = conn.streams.get_or_create_client_stream(1).expect("create");
        stream.recv_headers(false).expect("recv_headers"); // ボディ継続（Open）

        // conn/stream 両ウィンドウ（初期 65535）の過半を消費する。
        conn.recv_data_for_streaming(1, false, 40000).expect("recv");
        assert_eq!(conn.conn_recv_window, 65535 - 40000);
        assert_eq!(conn.streams.get_ref(1).unwrap().recv_window, 65535 - 40000);
        // 受信時点では WINDOW_UPDATE を送らない（write_buf にも直接書き込みにも出ない）。
        assert!(
            conn.write_buf.is_empty(),
            "受信時に WINDOW_UPDATE を積まない"
        );
        assert!(conn.stream.writes.is_empty(), "受信時に直接送信しない");

        // 消費連動の補充: conn（stream 0）/ stream 1 両レベルの WINDOW_UPDATE が
        // write_buf へ積まれ、ウィンドウが回復すること（送出は次回 flush）。
        conn.replenish_recv_window(1);
        let frames = parse_frames(&conn.write_buf);
        assert_eq!(frames.len(), 2, "conn + stream の WINDOW_UPDATE");
        assert!(frames
            .iter()
            .all(|(t, _, _, payload)| *t == FRAME_WINDOW_UPDATE && payload.len() == 4));
        assert!(frames.iter().any(|(_, _, sid, _)| *sid == 0));
        assert!(frames.iter().any(|(_, _, sid, _)| *sid == 1));
        assert_eq!(conn.conn_recv_window, 65535);
        assert_eq!(conn.streams.get_ref(1).unwrap().recv_window, 65535);

        // 消費が閾値（半分）未満なら何も積まれない（スパムしない）。
        conn.write_buf.clear();
        conn.recv_data_for_streaming(1, false, 100).expect("recv 2");
        conn.replenish_recv_window(1);
        assert!(conn.write_buf.is_empty(), "閾値未満では補充しない");
    }
}
