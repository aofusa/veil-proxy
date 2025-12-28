//! # HTTP/2 コネクション管理 (RFC 7540)
//!
//! HTTP/2 コネクションの確立、フレーム処理、ストリーム管理を行います。
//! monoio 非同期ランタイムと統合して動作します。

use std::collections::VecDeque;
use std::io;

use monoio::io::{AsyncReadRent, AsyncWriteRentExt};

use crate::http2::error::{Http2Error, Http2ErrorCode, Http2Result};
use crate::http2::frame::{Frame, FrameHeader, FrameEncoder, FrameDecoder};
use crate::http2::hpack::{HpackEncoder, HpackDecoder};
use crate::http2::settings::{Http2Settings, defaults};
use crate::http2::stream::{Stream, StreamState, StreamManager};

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
    /// 送信キュー
    #[allow(dead_code)]
    send_queue: VecDeque<Vec<u8>>,
}

impl<S> Http2Connection<S>
where
    S: AsyncReadRent + AsyncWriteRentExt + Unpin,
{
    /// 新しいコネクションを作成
    pub fn new(stream: S, settings: Http2Settings) -> Self {
        let hpack_decoder = HpackDecoder::new(settings.header_table_size as usize);
        let hpack_encoder = HpackEncoder::new(defaults::HEADER_TABLE_SIZE as usize);
        let frame_encoder = FrameEncoder::new(settings.max_frame_size);
        let frame_decoder = FrameDecoder::new(defaults::MAX_FRAME_SIZE);
        let streams = StreamManager::new(
            settings.max_concurrent_streams,
            settings.initial_window_size as i32,
        );
        
        // コネクションウィンドウサイズを設定から取得
        let conn_window = settings.connection_window_size as i32;

        Self {
            stream,
            local_settings: settings,
            remote_settings: Http2Settings::default(),
            streams,
            hpack_decoder,
            hpack_encoder,
            frame_encoder,
            frame_decoder,
            conn_send_window: conn_window,
            conn_recv_window: conn_window,
            goaway_sent: false,
            goaway_received: false,
            goaway_last_stream_id: None,
            settings_ack_pending: false,
            read_buf: vec![0u8; 65536],
            buf_start: 0,
            buf_end: 0,
            send_queue: VecDeque::new(),
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
            self.write_all(&frame).await?;
        }

        Ok(())
    }

    /// クライアントプリフェースを確認
    async fn expect_preface(&mut self) -> Http2Result<()> {
        let preface_len = CONNECTION_PREFACE.len();
        
        // プリフェースを読み込む
        while self.buf_end - self.buf_start < preface_len {
            self.read_more().await?;
        }

        // プリフェースを確認
        let received = &self.read_buf[self.buf_start..self.buf_start + preface_len];
        if received != CONNECTION_PREFACE {
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
        self.write_all(&frame).await?;
        self.settings_ack_pending = true;

        Ok(())
    }

    /// フレームを読み込み（外部からアクセス可能）
    /// 
    /// HTTP/2 フレームを1つ読み込んでデコードします。
    /// コネクションがクローズされた場合は ConnectionClosed エラーを返します。
    pub async fn read_frame(&mut self) -> Http2Result<Frame> {
        // フレームヘッダー (9 bytes) を確保
        while self.buf_end - self.buf_start < FrameHeader::SIZE {
            self.read_more().await?;
        }

        // ヘッダーをデコード
        let header = self.frame_decoder.decode_header(&self.read_buf[self.buf_start..])?;
        let total_len = FrameHeader::SIZE + header.length as usize;

        // ペイロードを確保
        while self.buf_end - self.buf_start < total_len {
            self.read_more().await?;
        }

        // フレームをデコード
        let payload_start = self.buf_start + FrameHeader::SIZE;
        let payload = &self.read_buf[payload_start..self.buf_start + total_len];
        let frame = self.frame_decoder.decode(&header, payload)?;

        self.buf_start += total_len;

        // バッファをコンパクト化
        if self.buf_start > 32768 {
            self.compact_buffer();
        }

        Ok(frame)
    }

    /// 追加データを読み込み
    async fn read_more(&mut self) -> Http2Result<()> {
        // バッファが不足している場合は拡張
        if self.buf_end >= self.read_buf.len() {
            if self.buf_start > 0 {
                self.compact_buffer();
            } else {
                // バッファを拡張
                self.read_buf.resize(self.read_buf.len() * 2, 0);
            }
        }

        // 読み込み用のスライスを準備
        let read_slice = std::mem::take(&mut self.read_buf);
        let (result, returned_buf) = self.stream.read(read_slice).await;
        self.read_buf = returned_buf;

        match result {
            Ok(0) => Err(Http2Error::ConnectionClosed),
            Ok(n) => {
                self.buf_end += n;
                Ok(())
            }
            Err(e) => Err(Http2Error::Io(e)),
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

    /// データを送信
    /// 
    /// monoio の write_all は成功時に全データ書き込みを保証するため、
    /// 成功時はループを抜ける実装が正しい。
    async fn write_all(&mut self, data: &[u8]) -> Http2Result<()> {
        let mut offset = 0;
        while offset < data.len() {
            let buf = data[offset..].to_vec();
            let buf_len = buf.len();
            let (result, _) = self.stream.write_all(buf).await;
            match result {
                Ok(_) => {
                    // monoio の write_all は成功時に全データ書き込みを保証
                    offset += buf_len;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(Http2Error::Io(e)),
            }
        }
        Ok(())
    }

    /// フレームを処理（外部からアクセス可能）
    /// 
    /// 受信したフレームを処理し、リクエストが完了した場合は ProcessedRequest を返します。
    pub async fn process_frame(&mut self, frame: Frame) -> Http2Result<Option<ProcessedRequest>> {
        match frame {
            Frame::Settings { ack, settings } => {
                self.handle_settings(ack, &settings).await?;
                Ok(None)
            }
            Frame::Headers { stream_id, end_stream, end_headers, priority, header_block } => {
                self.handle_headers(stream_id, end_stream, end_headers, priority, &header_block).await
            }
            Frame::Data { stream_id, end_stream, data } => {
                self.handle_data(stream_id, end_stream, &data).await
            }
            Frame::WindowUpdate { stream_id, increment } => {
                self.handle_window_update(stream_id, increment)?;
                Ok(None)
            }
            Frame::Ping { ack, data } => {
                self.handle_ping(ack, &data).await?;
                Ok(None)
            }
            Frame::GoAway { last_stream_id, error_code, debug_data } => {
                self.handle_goaway(last_stream_id, error_code, &debug_data)?;
                Ok(None)
            }
            Frame::RstStream { stream_id, error_code } => {
                self.handle_rst_stream(stream_id, error_code)?;
                Ok(None)
            }
            Frame::Priority { stream_id, priority } => {
                self.handle_priority(stream_id, priority)?;
                Ok(None)
            }
            Frame::Continuation { stream_id, end_headers, header_block } => {
                self.handle_continuation(stream_id, end_headers, &header_block).await
            }
            Frame::PushPromise { .. } => {
                // クライアントからの PUSH_PROMISE は無効
                Err(Http2Error::protocol_error("Client sent PUSH_PROMISE"))
            }
            Frame::Unknown { .. } => {
                // 未知のフレームは無視
                Ok(None)
            }
        }
    }

    /// SETTINGS フレームを処理
    async fn handle_settings(&mut self, ack: bool, settings: &[(u16, u32)]) -> Http2Result<()> {
        if ack {
            // ACK を受信
            self.settings_ack_pending = false;
            return Ok(());
        }

        // クライアントの設定を適用
        for &(id, value) in settings {
            match id {
                0x1 => {
                    // HEADER_TABLE_SIZE
                    self.hpack_encoder.set_max_table_size(value as usize);
                }
                0x2 => {
                    // ENABLE_PUSH (サーバーは無視)
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
                    if value < defaults::MAX_FRAME_SIZE || value > defaults::MAX_FRAME_SIZE_UPPER_LIMIT {
                        return Err(Http2Error::protocol_error("Invalid MAX_FRAME_SIZE"));
                    }
                    self.frame_encoder.set_max_frame_size(value);
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
        self.write_all(&ack_frame).await?;

        Ok(())
    }

    /// HEADERS フレームを処理
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

        // ストリームを取得または作成
        let stream = self.streams.get_or_create_client_stream(stream_id)?;

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
            self.decode_and_set_headers(stream_id)?;

            // リクエストが完了したかチェック
            if end_stream {
                return Ok(Some(ProcessedRequest { stream_id }));
            }
        } else {
            self.streams.set_receiving_headers(Some(stream_id));
        }

        Ok(None)
    }

    /// CONTINUATION フレームを処理
    async fn handle_continuation(
        &mut self,
        stream_id: u32,
        end_headers: bool,
        header_block: &[u8],
    ) -> Http2Result<Option<ProcessedRequest>> {
        // CONTINUATION 中でなければエラー
        let pending_id = self.streams.receiving_headers_stream()
            .ok_or_else(|| Http2Error::protocol_error("Unexpected CONTINUATION"))?;

        if pending_id != stream_id {
            return Err(Http2Error::protocol_error("CONTINUATION for wrong stream"));
        }

        let stream = self.streams.get(stream_id)
            .ok_or_else(|| Http2Error::stream_error(stream_id, Http2ErrorCode::StreamClosed, "Stream not found"))?;

        let end_stream = matches!(stream.state, StreamState::HalfClosedRemote);

        stream.append_header_fragment(header_block, end_headers);

        if end_headers {
            self.streams.set_receiving_headers(None);
            self.decode_and_set_headers(stream_id)?;

            if end_stream {
                return Ok(Some(ProcessedRequest { stream_id }));
            }
        }

        Ok(None)
    }

    /// ヘッダーブロックをデコードしてストリームに設定
    fn decode_and_set_headers(&mut self, stream_id: u32) -> Http2Result<()> {
        let stream = self.streams.get(stream_id)
            .ok_or_else(|| Http2Error::stream_error(stream_id, Http2ErrorCode::StreamClosed, "Stream not found"))?;

        let header_block = stream.take_header_block();
        let headers = self.hpack_decoder.decode(&header_block)
            .map_err(|e| Http2Error::compression_error(e.to_string()))?;

        let stream = self.streams.get(stream_id).unwrap();
        stream.request_headers = headers;

        // Content-Length を解析
        if let Some(cl) = stream.request_headers.iter().find(|h| h.name == b"content-length") {
            if let Some(len) = std::str::from_utf8(&cl.value).ok().and_then(|s| s.parse::<u64>().ok()) {
                stream.content_length = Some(len);
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
        let stream = self.streams.get(stream_id)
            .ok_or_else(|| Http2Error::stream_error(stream_id, Http2ErrorCode::StreamClosed, "Stream not found"))?;

        stream.recv_data(data, end_stream)?;

        // WINDOW_UPDATE を送信 (必要に応じて)
        self.maybe_send_window_update(stream_id).await?;

        if end_stream {
            Ok(Some(ProcessedRequest { stream_id }))
        } else {
            Ok(None)
        }
    }

    /// WINDOW_UPDATE を送信 (必要に応じて)
    async fn maybe_send_window_update(&mut self, stream_id: u32) -> Http2Result<()> {
        // コネクションレベル
        let conn_increment = defaults::CONNECTION_WINDOW_SIZE as i32 - self.conn_recv_window;
        if conn_increment > (defaults::CONNECTION_WINDOW_SIZE as i32 / 2) {
            let frame = self.frame_encoder.encode_window_update(0, conn_increment as u32);
            self.write_all(&frame).await?;
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
            let frame = self.frame_encoder.encode_window_update(stream_id, increment as u32);
            self.write_all(&frame).await?;
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

    /// PING を処理
    async fn handle_ping(&mut self, ack: bool, data: &[u8; 8]) -> Http2Result<()> {
        if !ack {
            // PING ACK を送信
            let frame = self.frame_encoder.encode_ping(data, true);
            self.write_all(&frame).await?;
        }
        Ok(())
    }

    /// GOAWAY を処理 (RFC 7540 Section 6.8)
    /// 
    /// GOAWAY 受信後は last_stream_id より大きい ID のストリームを
    /// 開始してはならない。
    fn handle_goaway(&mut self, last_stream_id: u32, error_code: u32, debug_data: &[u8]) -> Http2Result<()> {
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
            ftlog::debug!(
                "HTTP/2 GOAWAY received: last_stream_id={}",
                last_stream_id
            );
        }
        
        Ok(())
    }

    /// RST_STREAM を処理
    fn handle_rst_stream(&mut self, stream_id: u32, error_code: u32) -> Http2Result<()> {
        if let Some(stream) = self.streams.get(stream_id) {
            stream.recv_rst_stream(error_code);
        }
        Ok(())
    }

    /// PRIORITY を処理
    fn handle_priority(&mut self, stream_id: u32, priority: crate::http2::frame::types::PrioritySpec) -> Http2Result<()> {
        if let Some(stream) = self.streams.get(stream_id) {
            stream.dependency = priority.dependency;
            stream.weight = priority.weight;
            stream.exclusive = priority.exclusive;
        }
        Ok(())
    }

    /// レスポンスを送信
    /// 
    /// HTTP/2 RFC 7540 Section 8.1.2 に従い、ヘッダー名は小文字に変換されます。
    pub async fn send_response(
        &mut self,
        stream_id: u32,
        status: u16,
        headers: &[(&[u8], &[u8])],
        body: Option<&[u8]>,
    ) -> Http2Result<()> {
        // :status 用のバッファ（動的ステータスコード用）
        let mut status_buf = [0u8; 3];
        
        // ステータスコードを文字列に変換
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
                // 動的に生成（3桁のステータスコード）
                let s = status.min(999);
                status_buf[0] = b'0' + (s / 100) as u8;
                status_buf[1] = b'0' + ((s / 10) % 10) as u8;
                status_buf[2] = b'0' + (s % 10) as u8;
                &status_buf
            }
        };
        
        // HTTP/2 RFC 7540 Section 8.1.2:
        // "header field names MUST be converted to lowercase prior to their encoding in HTTP/2"
        // ヘッダー名を小文字に変換するためのストレージ
        let mut lowercase_names: Vec<Vec<u8>> = Vec::with_capacity(headers.len());
        for &(name, _) in headers {
            lowercase_names.push(name.to_ascii_lowercase());
        }
        
        // ステータスとヘッダーをエンコード
        let mut header_list: Vec<(&[u8], &[u8], bool)> = Vec::with_capacity(headers.len() + 1);
        header_list.push((b":status", status_str, false));
        
        // その他のヘッダー（小文字変換済み）
        for (i, &(_, value)) in headers.iter().enumerate() {
            header_list.push((&lowercase_names[i], value, false));
        }

        let end_stream = body.is_none() || body.map(|b| b.is_empty()).unwrap_or(true);
        let header_block = self.hpack_encoder.encode(&header_list)
            .map_err(|e| Http2Error::HpackEncode(e.to_string()))?;

        // HEADERS フレームを送信
        let headers_frame = self.frame_encoder.encode_headers(
            stream_id,
            &header_block,
            end_stream,
            true, // end_headers
            None,
        );
        self.write_all(&headers_frame).await?;

        // ストリーム状態を更新
        if let Some(stream) = self.streams.get(stream_id) {
            stream.send_headers(end_stream)?;
        }

        // ボディを送信
        if let Some(body) = body {
            if !body.is_empty() {
                self.send_data(stream_id, body, true).await?;
            }
        }

        Ok(())
    }

    /// DATA フレームを送信
    /// 
    /// フロー制御ウィンドウを考慮してデータを分割送信します。
    /// ウィンドウが不足した場合は WINDOW_UPDATE を待機します。
    pub async fn send_data(&mut self, stream_id: u32, data: &[u8], end_stream: bool) -> Http2Result<()> {
        let max_frame_size = self.remote_settings.max_frame_size as usize;
        let mut offset = 0;
        let mut window_update_wait_count = 0;
        const MAX_WINDOW_UPDATE_WAITS: usize = 100; // 無限ループ防止

        while offset < data.len() {
            // 送信可能な最大サイズを計算（フレームサイズとウィンドウの両方を考慮）
            let remaining = data.len() - offset;
            
            // ストリームウィンドウを取得
            let stream_window = self.streams.get_ref(stream_id)
                .map(|s| s.send_window)
                .unwrap_or(0);
            
            // コネクションとストリームの両方のウィンドウを考慮
            let available_window = self.conn_send_window.min(stream_window).max(0) as usize;
            
            if available_window == 0 {
                // ウィンドウが0の場合、WINDOW_UPDATEを待つ
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
                        if let Frame::WindowUpdate { stream_id: wid, increment } = frame {
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
                        if !matches!(e, Http2Error::Io(ref io_err) if io_err.kind() == io::ErrorKind::WouldBlock) {
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

            let frame = self.frame_encoder.encode_data(stream_id, chunk, end_stream && is_last);
            self.write_all(&frame).await?;

            offset += chunk_len;
            window_update_wait_count = 0; // 送信成功したのでリセット
        }

        // 状態更新
        if end_stream {
            if let Some(stream) = self.streams.get(stream_id) {
                stream.send_end_stream()?;
            }
        }

        Ok(())
    }

    /// GOAWAY を送信
    pub async fn send_goaway(&mut self, error_code: Http2ErrorCode, debug_data: &[u8]) -> Http2Result<()> {
        if self.goaway_sent {
            return Ok(());
        }

        let last_stream_id = self.streams.max_client_stream_id();
        let frame = self.frame_encoder.encode_goaway(last_stream_id, error_code as u32, debug_data);
        self.write_all(&frame).await?;
        self.goaway_sent = true;

        Ok(())
    }

    /// RST_STREAM を送信
    pub async fn send_rst_stream(&mut self, stream_id: u32, error_code: Http2ErrorCode) -> Http2Result<()> {
        let frame = self.frame_encoder.encode_rst_stream(stream_id, error_code as u32);
        self.write_all(&frame).await?;

        if let Some(stream) = self.streams.get(stream_id) {
            stream.send_rst_stream();
        }

        Ok(())
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
                    let _ = self.send_goaway(e.error_code(), e.to_string().as_bytes()).await;
                    return Err(e);
                }
            };

            // フレームを処理
            match self.process_frame(frame).await {
                Ok(Some(req)) => {
                    // リクエストが完了 - デフォルトレスポンスを送信
                    let headers: &[(&[u8], &[u8])] = &[
                        (b"content-type", b"text/plain"),
                        (b"server", b"veil/http2"),
                    ];
                    if let Err(e) = self.send_response(req.stream_id, 200, headers, Some(b"HTTP/2 OK")).await {
                        // ストリームエラーの場合は RST_STREAM を送信
                        if let Some(id) = e.rst_stream_id() {
                            let _ = self.send_rst_stream(id, e.error_code()).await;
                        } else if e.should_goaway() {
                            let _ = self.send_goaway(e.error_code(), e.to_string().as_bytes()).await;
                            return Err(e);
                        }
                    }
                }
                Ok(None) => {
                    // フレーム処理完了、次のフレームへ
                }
                Err(e) => {
                    if e.should_goaway() {
                        let _ = self.send_goaway(e.error_code(), e.to_string().as_bytes()).await;
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
}

/// 処理済みリクエスト
#[derive(Debug)]
pub struct ProcessedRequest {
    /// ストリーム ID
    pub stream_id: u32,
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
        let req = ProcessedRequest { stream_id: 1 };
        assert_eq!(req.stream_id, 1);
        
        let req2 = ProcessedRequest { stream_id: 3 };
        assert_eq!(req2.stream_id, 3);
    }

    #[test]
    fn test_processed_request_odd_stream_ids() {
        // クライアント開始ストリームは奇数ID
        let req = ProcessedRequest { stream_id: 1 };
        assert!(req.stream_id % 2 == 1);
        
        let req2 = ProcessedRequest { stream_id: 5 };
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
        let min_frame_size = 16384u32;  // 2^14
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

    // 注: 実際のコネクション処理のテストはモックTLSストリームが必要なため、
    // 統合テストとして別途実施することを推奨
}
