//! # HTTP/2 クライアント (H2C対応)
//!
//! HTTP/2 Prior Knowledge モードでバックエンドに接続するクライアント実装。
//! RFC 7540 Section 3.4 に基づく実装。

use crate::runtime::io::{AsyncReadRent, AsyncWriteRentExt};
use std::io;

use crate::http2::error::{Http2Error, Http2Result};
use crate::http2::frame::{Frame, FrameDecoder, FrameEncoder, FrameHeader};
use crate::http2::hpack::{HpackDecoder, HpackEncoder};
use crate::http2::settings::{defaults, Http2Settings};

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
        let hpack_decoder = HpackDecoder::new(settings.header_table_size as usize);
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
        self.write_all(CONNECTION_PREFACE.to_vec()).await?;

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
        self.write_all(frame).await?;
        self.settings_ack_pending = true;

        Ok(())
    }

    /// サーバー SETTINGS を受信
    async fn receive_server_settings(&mut self) -> Http2Result<()> {
        let frame = self.read_frame().await?;

        match frame {
            Frame::Settings {
                ack: false,
                settings,
            } => {
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
                self.write_all(ack_frame).await?;
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
                    self.write_all(ping_ack).await?;
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
            // B-11: expect はプロキシが終端する（ボディを全量送信するため、バックエンドに
            // 100 Continue 中間応答を出させない）。
            if name.starts_with(b":")
                || name.eq_ignore_ascii_case(b"connection")
                || name.eq_ignore_ascii_case(b"keep-alive")
                || name.eq_ignore_ascii_case(b"transfer-encoding")
                || name.eq_ignore_ascii_case(b"host")
                || name.eq_ignore_ascii_case(b"expect")
            {
                continue;
            }
            header_list.push((name, value, false));
        }

        let end_stream = body.is_none() || body.map(|b| b.is_empty()).unwrap_or(true);
        let header_block = self
            .hpack_encoder
            .encode(&header_list)
            .map_err(|e| Http2Error::HpackEncode(e.to_string()))?;

        // HEADERS フレームを送信
        let headers_frame = self.frame_encoder.encode_headers(
            stream_id,
            &header_block,
            end_stream,
            true, // end_headers
            None,
        );
        self.write_all(headers_frame).await?;

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
    async fn send_data(
        &mut self,
        stream_id: u32,
        data: &[u8],
        end_stream: bool,
    ) -> Http2Result<()> {
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

            let frame = self
                .frame_encoder
                .encode_data(stream_id, chunk, end_stream && is_last);
            self.write_all(frame).await?;

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
            trailers: Vec::new(),
        };

        let mut headers_received = false;

        loop {
            let frame = self.read_frame().await?;

            match frame {
                Frame::Headers {
                    stream_id: sid,
                    end_stream,
                    end_headers: _,
                    header_block,
                    ..
                } => {
                    if sid != stream_id {
                        continue;
                    }

                    // ヘッダーをデコード
                    let headers = self
                        .hpack_decoder
                        .decode(&header_block)
                        .map_err(|e| Http2Error::compression_error(e.to_string()))?;

                    if !headers_received {
                        // 初回ヘッダー
                        headers_received = true;
                        for header in headers {
                            if header.name == b":status" {
                                if let Ok(s) = std::str::from_utf8(&header.value) {
                                    response.status = s.parse().unwrap_or(0);
                                }
                            } else if !header.name.starts_with(b":") {
                                response.headers.push((header.name, header.value));
                            }
                        }
                    } else {
                        // トレイラー (2回目以降の HEADERS フレーム)
                        for header in headers {
                            response.trailers.push((header.name, header.value));
                        }
                    }

                    if end_stream {
                        return Ok(response);
                    }
                }
                Frame::Data {
                    stream_id: sid,
                    end_stream,
                    data,
                } => {
                    if sid != stream_id {
                        continue;
                    }

                    response.body.extend_from_slice(&data);

                    // フロー制御
                    let data_len = data.len() as i32;
                    self.conn_recv_window -= data_len;

                    // WINDOW_UPDATE を送信
                    if self.conn_recv_window < (defaults::CONNECTION_WINDOW_SIZE as i32 / 2) {
                        let increment =
                            defaults::CONNECTION_WINDOW_SIZE as i32 - self.conn_recv_window;
                        let wu_frame = self.frame_encoder.encode_window_update(0, increment as u32);
                        self.write_all(wu_frame).await?;
                        self.conn_recv_window += increment;

                        // ストリームレベルも
                        let wu_stream = self
                            .frame_encoder
                            .encode_window_update(stream_id, increment as u32);
                        self.write_all(wu_stream).await?;
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
                    self.write_all(ping_ack).await?;
                }
                Frame::Settings {
                    ack: false,
                    settings,
                } => {
                    // SETTINGS ACK を送信
                    for &(id, value) in &settings {
                        match id {
                            0x1 => self.hpack_encoder.set_max_table_size(value as usize),
                            0x4 => self.remote_settings.initial_window_size = value,
                            0x5 => self.remote_settings.max_frame_size = value,
                            _ => {}
                        }
                    }
                    let ack_frame = self.frame_encoder.encode_settings_ack();
                    self.write_all(ack_frame).await?;
                }
                Frame::GoAway { .. } => {
                    return Err(Http2Error::ConnectionClosed);
                }
                Frame::RstStream {
                    stream_id: sid,
                    error_code,
                } => {
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
        let header = self
            .frame_decoder
            .decode_header(&self.read_buf[self.buf_start..])?;
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

        // buf_end 以降の領域へ「追記」する。RECV は write_ptr=バッファ先頭に書き込むため、
        // バッファ全体を渡して offset 0 から上書きすると、未パースの部分フレーム
        // ([buf_start..buf_end]) を破壊し、かつ buf_end += n で長さも誤る（フレームが複数回の
        // read にまたがる場合に発生）。tail を切り出して渡し、実際に読み込んだ n バイトのみ
        // 結合する（返却 len は IoBufMut::set_init が grow-only のため信用しない）。
        let mut full_buf = std::mem::take(&mut self.read_buf);
        let tail_buf = full_buf.split_off(self.buf_end);
        let (result, returned_tail) = self.stream.read(tail_buf).await;

        match result {
            Ok(0) => {
                self.read_buf = full_buf;
                Err(Http2Error::ConnectionClosed)
            }
            Ok(n) => {
                full_buf.extend_from_slice(&returned_tail[..n]);
                self.read_buf = full_buf;
                self.buf_end += n;
                Ok(())
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
    /// 従来の per-frame `data[offset..].to_vec()` に由来するアロケーション + 全コピーを
    /// 排除する（proxy→バックエンド方向 HTTP/2 送信ホットパス最適化・F-73 残件）。
    /// runtime の write_all は short write を内部で継続する（B-27）ため、`Ok` は常に
    /// 完全書き込みを意味する。
    async fn write_all(&mut self, mut buf: Vec<u8>) -> Http2Result<()> {
        loop {
            let (result, returned) = self.stream.write_all(buf).await;
            match result {
                Ok(_) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    buf = returned;
                    continue;
                }
                Err(e) => return Err(Http2Error::Io(e)),
            }
        }
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
    /// レスポンストレイラー (gRPC用)
    pub trailers: Vec<(Vec<u8>, Vec<u8>)>,
}

/// H2C gRPC レスポンス
#[cfg(feature = "grpc")]
pub struct H2cGrpcResponse {
    /// HTTP ステータスコード (通常 200)
    pub http_status: u16,
    /// gRPC ステータスコード (from grpc-status trailer)
    pub grpc_status: u32,
    /// gRPC エラーメッセージ (from grpc-message trailer)
    pub grpc_message: Option<String>,
    /// レスポンスヘッダー
    pub headers: Vec<(Vec<u8>, Vec<u8>)>,
    /// レスポンスボディ (gRPC framed)
    pub body: Vec<u8>,
    /// トレイラー
    pub trailers: Vec<(Vec<u8>, Vec<u8>)>,
}

#[cfg(feature = "grpc")]
impl<S> H2cClient<S>
where
    S: AsyncReadRent + AsyncWriteRentExt + Unpin,
{
    /// gRPC リクエストを送信してレスポンスを受信
    ///
    /// # 引数
    /// * `service_method` - gRPC サービス/メソッドパス (e.g., "/package.Service/Method")
    /// * `authority` - ホスト名
    /// * `message` - gRPC メッセージ（未フレーム化）
    /// * `timeout` - タイムアウト（オプション）
    pub async fn send_grpc_request(
        &mut self,
        service_method: &[u8],
        authority: &[u8],
        message: &[u8],
        timeout: Option<std::time::Duration>,
    ) -> Http2Result<H2cGrpcResponse> {
        use crate::grpc::framing::GrpcFrame;
        use crate::grpc::headers::format_grpc_timeout;

        let stream_id = self.next_stream_id;
        self.next_stream_id += 2;

        // gRPC メッセージをフレーム化 (5-byte header + message)
        let framed_message = GrpcFrame::new(message.to_vec()).encode();

        // ヘッダーリストを構築
        let mut header_list: Vec<(&[u8], &[u8], bool)> = Vec::with_capacity(8);
        header_list.push((b":method", b"POST", false));
        header_list.push((b":path", service_method, false));
        header_list.push((b":scheme", b"http", false));
        header_list.push((b":authority", authority, false));
        header_list.push((b"content-type", b"application/grpc+proto", false));
        header_list.push((b"te", b"trailers", false));

        // タイムアウトヘッダー
        let timeout_str;
        if let Some(t) = timeout {
            timeout_str = format_grpc_timeout(t);
            header_list.push((b"grpc-timeout", timeout_str.as_bytes(), false));
        }

        let header_block = self
            .hpack_encoder
            .encode(&header_list)
            .map_err(|e| Http2Error::HpackEncode(e.to_string()))?;

        // HEADERS フレームを送信 (end_stream=false, body follows)
        let headers_frame =
            self.frame_encoder
                .encode_headers(stream_id, &header_block, false, true, None);
        self.write_all(headers_frame).await?;

        // DATA フレームを送信 (end_stream=true)
        self.send_data(stream_id, &framed_message, true).await?;

        // gRPC レスポンスを受信
        self.receive_grpc_response(stream_id).await
    }

    /// gRPC レスポンスを受信
    async fn receive_grpc_response(&mut self, stream_id: u32) -> Http2Result<H2cGrpcResponse> {
        use crate::grpc::status::GrpcStatus;

        let mut response = H2cGrpcResponse {
            http_status: 0,
            grpc_status: 0,
            grpc_message: None,
            headers: Vec::new(),
            body: Vec::new(),
            trailers: Vec::new(),
        };

        let mut headers_received = false;

        loop {
            let frame = self.read_frame().await?;

            match frame {
                Frame::Headers {
                    stream_id: sid,
                    end_stream,
                    end_headers: _,
                    header_block,
                    ..
                } => {
                    if sid != stream_id {
                        continue;
                    }

                    let headers = self
                        .hpack_decoder
                        .decode(&header_block)
                        .map_err(|e| Http2Error::compression_error(e.to_string()))?;

                    if !headers_received {
                        // Initial headers
                        headers_received = true;
                        for header in headers {
                            if header.name == b":status" {
                                if let Ok(s) = std::str::from_utf8(&header.value) {
                                    response.http_status = s.parse().unwrap_or(0);
                                }
                            } else if !header.name.starts_with(b":") {
                                response.headers.push((header.name, header.value));
                            }
                        }
                    } else {
                        // Trailers (second HEADERS frame)
                        for header in headers {
                            if header.name == b"grpc-status" {
                                if let Ok(s) = std::str::from_utf8(&header.value) {
                                    response.grpc_status = s.parse().unwrap_or(2);
                                    // Unknown
                                }
                            } else if header.name == b"grpc-message" {
                                response.grpc_message = GrpcStatus::decode_message(&header.value);
                            } else {
                                response.trailers.push((header.name, header.value));
                            }
                        }
                    }

                    if end_stream {
                        return Ok(response);
                    }
                }
                Frame::Data {
                    stream_id: sid,
                    end_stream,
                    data,
                } => {
                    if sid != stream_id {
                        continue;
                    }

                    response.body.extend_from_slice(&data);

                    // フロー制御
                    let data_len = data.len() as i32;
                    self.conn_recv_window -= data_len;

                    if self.conn_recv_window < (defaults::CONNECTION_WINDOW_SIZE as i32 / 2) {
                        let increment =
                            defaults::CONNECTION_WINDOW_SIZE as i32 - self.conn_recv_window;
                        let wu_frame = self.frame_encoder.encode_window_update(0, increment as u32);
                        self.write_all(wu_frame).await?;
                        self.conn_recv_window += increment;

                        let wu_stream = self
                            .frame_encoder
                            .encode_window_update(stream_id, increment as u32);
                        self.write_all(wu_stream).await?;
                    }

                    if end_stream {
                        return Ok(response);
                    }
                }
                Frame::WindowUpdate { .. } => {}
                Frame::Ping { ack: false, data } => {
                    let ping_ack = self.frame_encoder.encode_ping(&data, true);
                    self.write_all(ping_ack).await?;
                }
                Frame::Settings {
                    ack: false,
                    settings,
                } => {
                    for &(id, value) in &settings {
                        match id {
                            0x1 => self.hpack_encoder.set_max_table_size(value as usize),
                            0x4 => self.remote_settings.initial_window_size = value,
                            0x5 => self.remote_settings.max_frame_size = value,
                            _ => {}
                        }
                    }
                    let ack_frame = self.frame_encoder.encode_settings_ack();
                    self.write_all(ack_frame).await?;
                }
                Frame::GoAway { .. } => {
                    return Err(Http2Error::ConnectionClosed);
                }
                Frame::RstStream {
                    stream_id: sid,
                    error_code,
                } => {
                    if sid == stream_id {
                        return Err(Http2Error::stream_closed(stream_id, error_code));
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! H2C クライアント送信ホットパスの検証
    //!
    //! F-73 残件（proxy→バックエンド方向 HTTP/2 送信）で `write_all` を所有 `Vec<u8>` の
    //! ムーブ委譲へ変更した。送出バイト列が RFC 7540 準拠のフレーム列として正しいこと
    //! （＝ゼロコピー化でバイト内容を壊していないこと）と、スクリプト応答を正しく
    //! 受信・パースできることをモックストリームで検証する。

    use super::*;
    use crate::http2::frame::FrameEncoder;
    use crate::http2::hpack::HpackEncoder;
    use crate::runtime::buf::{IoBuf, IoBufMut};
    use crate::runtime::io::{AsyncReadRent, AsyncWriteRent, BufResult};
    use std::future::Future;

    /// 送出バイト列を記録しつつ、スクリプト応答を read で返すモックストリーム。
    struct ScriptedStream {
        writes: Vec<Vec<u8>>,
        read_data: Vec<u8>,
        read_pos: usize,
    }

    impl ScriptedStream {
        fn new(read_data: Vec<u8>) -> Self {
            Self {
                writes: Vec::new(),
                read_data,
                read_pos: 0,
            }
        }

        fn written(&self) -> Vec<u8> {
            self.writes.iter().flatten().copied().collect()
        }
    }

    impl AsyncReadRent for ScriptedStream {
        async fn read<T: IoBufMut>(&mut self, mut buf: T) -> BufResult<usize, T> {
            let remaining = self.read_data.len() - self.read_pos;
            let cap = buf.bytes_total();
            let n = remaining.min(cap);
            if n > 0 {
                // SAFETY: write_ptr()..cap は buf の書き込み可能領域。n<=cap。
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        self.read_data.as_ptr().add(self.read_pos),
                        buf.write_ptr(),
                        n,
                    );
                    buf.set_init(n);
                }
                self.read_pos += n;
            }
            (Ok(n), buf)
        }
    }

    impl AsyncWriteRent for ScriptedStream {
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

    /// io_uring 不要な同期ドライバ（Pending は自己 wake 前提で即再試行）。
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
            out.push((ftype, flags, sid, bytes[i + 9..i + 9 + len].to_vec()));
            i += 9 + len;
        }
        assert_eq!(i, bytes.len(), "trailing bytes not frame-aligned");
        out
    }

    /// サーバー応答（HEADERS :status 200 + DATA END_STREAM）をライブラリ自身の
    /// エンコーダで構築する。
    fn build_response(body: &[u8]) -> Vec<u8> {
        let mut hpack = HpackEncoder::new(4096);
        let enc = FrameEncoder::new(16384);
        let header_block = hpack
            .encode(&[
                (b":status", b"200", false),
                (b"content-type", b"text/plain", false),
            ])
            .expect("encode response headers");
        let mut out = enc.encode_headers(1, &header_block, false, true, None);
        out.extend_from_slice(&enc.encode_data(1, body, true));
        out
    }

    #[test]
    fn send_request_emits_correct_frames_and_parses_response() {
        let resp_body = b"pong";
        let mut client = H2cClient::new(
            ScriptedStream::new(build_response(resp_body)),
            Http2Settings::default(),
        );

        let req_body = b"ping-body";
        let resp = drive(client.send_request(
            b"POST",
            b"/echo",
            b"backend.local",
            &[(b"x-test", b"1")],
            Some(req_body),
        ))
        .expect("send_request");

        // 応答が正しくパースされる（ゼロコピー化した送信経路でも往復が成立する）。
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, resp_body);
        assert!(resp
            .headers
            .iter()
            .any(|(n, v)| n == b"content-type" && v == b"text/plain"));

        // 送出フレーム: HEADERS(END_HEADERS) + DATA(END_STREAM)。
        let frames = parse_frames(&client.stream.written());
        assert_eq!(frames.len(), 2, "expected HEADERS + DATA, got {frames:?}");

        let (htype, hflags, hsid, _) = &frames[0];
        assert_eq!(*htype, 0x1, "first frame is HEADERS");
        assert_eq!(*hsid, 1, "client stream id starts at 1");
        assert_eq!(hflags & 0x4, 0x4, "END_HEADERS set");
        assert_eq!(hflags & 0x1, 0x0, "END_STREAM not set (body follows)");

        let (dtype, dflags, dsid, dpayload) = &frames[1];
        assert_eq!(*dtype, 0x0, "second frame is DATA");
        assert_eq!(*dsid, 1);
        assert_eq!(dflags & 0x1, 0x1, "END_STREAM set on last DATA");
        assert_eq!(dpayload.as_slice(), req_body, "DATA payload byte-identical");
    }

    // ====================
    // F-67: HTTP/2 上流（バックエンド）プロトコル違反への耐性
    // ====================
    //
    // 不正な h2 バックエンド応答（早期切断・切り詰めフレーム・ゴミバイト・GOAWAY・
    // RST_STREAM）に対し、H2C クライアントが **panic・無限ループせず必ず Err を返す**
    // ことを検証する（クラッシュ／ハングでのクライアント可視デシンクを防ぐ）。

    /// バックエンドが応答を返さず即切断（EOF）→ ConnectionClosed で速やかに Err。
    #[test]
    fn backend_immediate_eof_returns_error() {
        let mut client = H2cClient::new(ScriptedStream::new(Vec::new()), Http2Settings::default());
        let result = drive(client.send_request(b"GET", b"/", b"b", &[], None));
        assert!(
            result.is_err(),
            "immediate EOF must yield Err, not hang/panic"
        );
    }

    /// フレームヘッダは完全だが宣言長ぶんのペイロードが届かず EOF（切り詰め）→ Err。
    #[test]
    fn backend_truncated_frame_returns_error() {
        let full = build_response(b"body");
        // HEADERS フレームヘッダ(9B)+数バイトだけ渡して切り詰める（宣言長に満たない）。
        let truncated = full[..12.min(full.len())].to_vec();
        let mut client = H2cClient::new(ScriptedStream::new(truncated), Http2Settings::default());
        let result = drive(client.send_request(b"GET", b"/", b"b", &[], None));
        assert!(result.is_err(), "truncated frame must yield Err, not hang");
    }

    /// フレームとして解釈不能なゴミバイト列 → panic せず Err。
    #[test]
    fn backend_garbage_bytes_returns_error() {
        let garbage = vec![0xffu8; 64];
        let mut client = H2cClient::new(ScriptedStream::new(garbage), Http2Settings::default());
        let result = drive(client.send_request(b"GET", b"/", b"b", &[], None));
        assert!(result.is_err(), "garbage bytes must yield Err, not panic");
    }

    /// バックエンドが GOAWAY を返す → ConnectionClosed で Err。
    #[test]
    fn backend_goaway_returns_error() {
        let enc = FrameEncoder::new(16384);
        let goaway = enc.encode_goaway(0, 0, b"");
        let mut client = H2cClient::new(ScriptedStream::new(goaway), Http2Settings::default());
        let result = drive(client.send_request(b"GET", b"/", b"b", &[], None));
        assert!(matches!(result, Err(Http2Error::ConnectionClosed)));
    }

    /// バックエンドが対象ストリームへ RST_STREAM を返す → stream error で Err。
    #[test]
    fn backend_rst_stream_returns_error() {
        let enc = FrameEncoder::new(16384);
        // クライアントの最初のストリーム ID は 1。
        let rst = enc.encode_rst_stream(1, 0x8 /* CANCEL */);
        let mut client = H2cClient::new(ScriptedStream::new(rst), Http2Settings::default());
        let result = drive(client.send_request(b"GET", b"/", b"b", &[], None));
        assert!(result.is_err(), "RST_STREAM must yield Err");
    }
}
