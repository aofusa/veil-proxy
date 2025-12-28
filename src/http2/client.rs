//! # HTTP/2 クライアント (H2C対応)
//!
//! HTTP/2 Prior Knowledge モードでバックエンドに接続するクライアント実装。
//! RFC 7540 Section 3.4 に基づく実装。

use std::io;
use monoio::io::{AsyncReadRent, AsyncWriteRentExt};

use crate::http2::error::{Http2Error, Http2Result};
use crate::http2::frame::{Frame, FrameHeader, FrameEncoder, FrameDecoder};
use crate::http2::hpack::{HpackEncoder, HpackDecoder};
use crate::http2::settings::{Http2Settings, defaults};

/// HTTP/2 コネクションプリフェース (クライアントが送信)
pub const CONNECTION_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// H2C クライアント接続
pub struct H2cClient<S> {
    /// TCP ストリーム
    stream: S,
    /// ローカル設定 (クライアント)
    local_settings: Http2Settings,
    /// リモート設定 (サーバー)
    remote_settings: Http2Settings,
    /// HPACK エンコーダ
    hpack_encoder: HpackEncoder,
    /// HPACK デコーダ
    hpack_decoder: HpackDecoder,
    /// フレームエンコーダ
    frame_encoder: FrameEncoder,
    /// フレームデコーダ
    frame_decoder: FrameDecoder,
    /// 読み込みバッファ
    read_buf: Vec<u8>,
    /// バッファ内の有効データ開始位置
    buf_start: usize,
    /// バッファ内の有効データ終了位置
    buf_end: usize,
    /// コネクションレベル送信ウィンドウ
    conn_send_window: i32,
    /// コネクションレベル受信ウィンドウ
    conn_recv_window: i32,
    /// 次のストリームID (クライアントは奇数)
    next_stream_id: u32,
    /// SETTINGS ACK 待ち
    settings_ack_pending: bool,
}

impl<S> H2cClient<S>
where
    S: AsyncReadRent + AsyncWriteRentExt + Unpin,
{
    /// 新しいH2Cクライアントを作成
    pub fn new(stream: S, settings: Http2Settings) -> Self {
        let hpack_encoder = HpackEncoder::new(settings.header_table_size as usize);
        let hpack_decoder = HpackDecoder::new(defaults::HEADER_TABLE_SIZE as usize);
        let frame_encoder = FrameEncoder::new(settings.max_frame_size);
        let frame_decoder = FrameDecoder::new(defaults::MAX_FRAME_SIZE);

        Self {
            stream,
            local_settings: settings,
            remote_settings: Http2Settings::default(),
            hpack_encoder,
            hpack_decoder,
            frame_encoder,
            frame_decoder,
            read_buf: vec![0u8; 65536],
            buf_start: 0,
            buf_end: 0,
            conn_send_window: defaults::CONNECTION_WINDOW_SIZE as i32,
            conn_recv_window: defaults::CONNECTION_WINDOW_SIZE as i32,
            next_stream_id: 1, // クライアントは奇数
            settings_ack_pending: false,
        }
    }

    /// HTTP/2 ハンドシェイクを実行 (クライアント側)
    ///
    /// 1. コネクションプリフェースを送信
    /// 2. SETTINGS を送信
    /// 3. サーバー SETTINGS を受信
    /// 4. SETTINGS ACK を送受信
    pub async fn handshake(&mut self) -> Http2Result<()> {
        // 1. コネクションプリフェースを送信
        self.write_all(CONNECTION_PREFACE).await?;

        // 2. クライアント SETTINGS を送信
        self.send_settings().await?;

        // 3. サーバー SETTINGS を受信して ACK
        self.receive_server_settings().await?;

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

    /// サーバー SETTINGS を受信
    async fn receive_server_settings(&mut self) -> Http2Result<()> {
        let frame = self.read_frame().await?;

        match frame {
            Frame::Settings { ack: false, settings } => {
                // サーバー設定を適用
                for &(id, value) in &settings {
                    match id {
                        0x1 => self.hpack_encoder.set_max_table_size(value as usize),
                        0x4 => self.remote_settings.initial_window_size = value,
                        0x5 => {
                            self.frame_encoder.set_max_frame_size(value);
                            self.remote_settings.max_frame_size = value;
                        }
                        _ => {}
                    }
                }

                // SETTINGS ACK を送信
                let ack_frame = self.frame_encoder.encode_settings_ack();
                self.write_all(&ack_frame).await?;
            }
            _ => {
                return Err(Http2Error::protocol_error("Expected SETTINGS frame"));
            }
        }

        // サーバーからの SETTINGS ACK を待つ
        loop {
            let frame = self.read_frame().await?;
            match frame {
                Frame::Settings { ack: true, .. } => {
                    self.settings_ack_pending = false;
                    break;
                }
                Frame::WindowUpdate { .. } => {
                    // ウィンドウ更新は無視
                }
                Frame::Ping { ack: false, data } => {
                    // PING ACK を送信
                    let ping_ack = self.frame_encoder.encode_ping(&data, true);
                    self.write_all(&ping_ack).await?;
                }
                _ => {
                    // 他のフレームはスキップ
                }
            }
        }

        Ok(())
    }

    /// リクエストを送信してレスポンスを受信
    pub async fn send_request(
        &mut self,
        method: &[u8],
        path: &[u8],
        authority: &[u8],
        headers: &[(&[u8], &[u8])],
        body: Option<&[u8]>,
    ) -> Http2Result<H2cResponse> {
        let stream_id = self.next_stream_id;
        self.next_stream_id += 2;

        // ヘッダーリストを構築
        let mut header_list: Vec<(&[u8], &[u8], bool)> = Vec::with_capacity(headers.len() + 4);
        header_list.push((b":method", method, false));
        header_list.push((b":path", path, false));
        header_list.push((b":scheme", b"http", false));
        header_list.push((b":authority", authority, false));

        for &(name, value) in headers {
            // 疑似ヘッダーとホップバイホップヘッダーをスキップ
            if name.starts_with(b":") 
                || name.eq_ignore_ascii_case(b"connection")
                || name.eq_ignore_ascii_case(b"keep-alive")
                || name.eq_ignore_ascii_case(b"transfer-encoding")
                || name.eq_ignore_ascii_case(b"host")
            {
                continue;
            }
            header_list.push((name, value, false));
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

        // ボディを送信
        if let Some(body_data) = body {
            if !body_data.is_empty() {
                self.send_data(stream_id, body_data, true).await?;
            }
        }

        // レスポンスを受信
        self.receive_response(stream_id).await
    }

    /// DATA フレームを送信
    async fn send_data(&mut self, stream_id: u32, data: &[u8], end_stream: bool) -> Http2Result<()> {
        let max_frame_size = self.remote_settings.max_frame_size as usize;
        let mut offset = 0;

        while offset < data.len() {
            let chunk_len = (data.len() - offset).min(max_frame_size);
            let is_last = offset + chunk_len >= data.len();
            let chunk = &data[offset..offset + chunk_len];

            // フロー制御チェック
            let len = chunk.len() as i32;
            if len > self.conn_send_window {
                return Err(Http2Error::protocol_error("Send window exhausted"));
            }
            self.conn_send_window -= len;

            let frame = self.frame_encoder.encode_data(stream_id, chunk, end_stream && is_last);
            self.write_all(&frame).await?;

            offset += chunk_len;
        }

        Ok(())
    }

    /// レスポンスを受信
    async fn receive_response(&mut self, stream_id: u32) -> Http2Result<H2cResponse> {
        let mut response = H2cResponse {
            status: 0,
            headers: Vec::new(),
            body: Vec::new(),
        };

        loop {
            let frame = self.read_frame().await?;

            match frame {
                Frame::Headers { stream_id: sid, end_stream, end_headers, header_block, .. } => {
                    if sid != stream_id {
                        continue;
                    }

                    // ヘッダーをデコード
                    let headers = self.hpack_decoder.decode(&header_block)
                        .map_err(|e| Http2Error::compression_error(e.to_string()))?;

                    for header in headers {
                        if header.name == b":status" {
                            if let Ok(s) = std::str::from_utf8(&header.value) {
                                response.status = s.parse().unwrap_or(0);
                            }
                        } else if !header.name.starts_with(b":") {
                            response.headers.push((header.name, header.value));
                        }
                    }

                    if end_stream {
                        return Ok(response);
                    }
                    
                    if !end_headers {
                        // CONTINUATION を待つ
                        // 簡略化のため、end_headers = true を前提
                    }
                }
                Frame::Data { stream_id: sid, end_stream, data } => {
                    if sid != stream_id {
                        continue;
                    }

                    response.body.extend_from_slice(&data);

                    // フロー制御
                    let data_len = data.len() as i32;
                    self.conn_recv_window -= data_len;

                    // WINDOW_UPDATE を送信
                    if self.conn_recv_window < (defaults::CONNECTION_WINDOW_SIZE as i32 / 2) {
                        let increment = defaults::CONNECTION_WINDOW_SIZE as i32 - self.conn_recv_window;
                        let wu_frame = self.frame_encoder.encode_window_update(0, increment as u32);
                        self.write_all(&wu_frame).await?;
                        self.conn_recv_window += increment;

                        // ストリームレベルも
                        let wu_stream = self.frame_encoder.encode_window_update(stream_id, increment as u32);
                        self.write_all(&wu_stream).await?;
                    }

                    if end_stream {
                        return Ok(response);
                    }
                }
                Frame::WindowUpdate { .. } => {
                    // ウィンドウ更新を処理
                }
                Frame::Ping { ack: false, data } => {
                    // PING ACK を送信
                    let ping_ack = self.frame_encoder.encode_ping(&data, true);
                    self.write_all(&ping_ack).await?;
                }
                Frame::Settings { ack: false, settings } => {
                    // SETTINGS ACK を送信
                    for &(id, value) in &settings {
                        match id {
                            0x4 => self.remote_settings.initial_window_size = value,
                            0x5 => self.remote_settings.max_frame_size = value,
                            _ => {}
                        }
                    }
                    let ack_frame = self.frame_encoder.encode_settings_ack();
                    self.write_all(&ack_frame).await?;
                }
                Frame::GoAway { .. } => {
                    return Err(Http2Error::ConnectionClosed);
                }
                Frame::RstStream { stream_id: sid, error_code } => {
                    if sid == stream_id {
                        return Err(Http2Error::stream_closed(stream_id, error_code));
                    }
                }
                _ => {}
            }
        }
    }

    /// フレームを読み込み
    async fn read_frame(&mut self) -> Http2Result<Frame> {
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
                self.read_buf.resize(self.read_buf.len() * 2, 0);
            }
        }

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
}

/// H2C レスポンス
pub struct H2cResponse {
    /// ステータスコード
    pub status: u16,
    /// レスポンスヘッダー
    pub headers: Vec<(Vec<u8>, Vec<u8>)>,
    /// レスポンスボディ
    pub body: Vec<u8>,
}
