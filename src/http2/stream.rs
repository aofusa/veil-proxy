//! # HTTP/2 ストリーム管理 (RFC 7540 Section 5)
//!
//! HTTP/2 ストリームの状態管理とフロー制御を実装します。

use std::collections::HashMap;
use crate::http2::error::{Http2Error, Http2ErrorCode};
use crate::http2::hpack::HeaderField;

/// ストリーム状態 (RFC 7540 Section 5.1)
///
/// ```text
///                          +--------+
///                  send PP |        | recv PP
///                 ,--------|  idle  |--------.
///                /         |        |         \
///               v          +--------+          v
///        +----------+          |           +----------+
///        |          |          | send H /  |          |
/// ,------| reserved |          | recv H    | reserved |------.
/// |      | (local)  |          |           | (remote) |      |
/// |      +----------+          v           +----------+      |
/// |          |             +--------+             |          |
/// |          |     recv ES |        | send ES     |          |
/// |   send H |     ,-------|  open  |-------.     | recv H   |
/// |          |    /        |        |        \    |          |
/// |          v   v         +--------+         v   v          |
/// |      +----------+          |           +----------+      |
/// |      |   half   |          |           |   half   |      |
/// |      |  closed  |          | send R /  |  closed  |      |
/// |      | (remote) |          | recv R    | (local)  |      |
/// |      +----------+          |           +----------+      |
/// |           |                |                 |           |
/// |           | send ES /      |       recv ES / |           |
/// |           | send R /       v        send R / |           |
/// |           | recv R     +--------+   recv R   |           |
/// | send R /  `----------->|        |<-----------'  send R / |
/// | recv R                 | closed |               recv R   |
/// `----------------------->|        |<-----------------------'
///                          +--------+
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamState {
    /// 初期状態
    Idle,
    /// ローカルで予約済み (PUSH_PROMISE 送信後)
    ReservedLocal,
    /// リモートで予約済み (PUSH_PROMISE 受信後)
    ReservedRemote,
    /// オープン (双方向でデータ送受信可能)
    Open,
    /// ローカル側がクローズ (送信終了)
    HalfClosedLocal,
    /// リモート側がクローズ (受信終了)
    HalfClosedRemote,
    /// 完全にクローズ
    Closed,
}

impl std::fmt::Display for StreamState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Idle => "idle",
            Self::ReservedLocal => "reserved (local)",
            Self::ReservedRemote => "reserved (remote)",
            Self::Open => "open",
            Self::HalfClosedLocal => "half-closed (local)",
            Self::HalfClosedRemote => "half-closed (remote)",
            Self::Closed => "closed",
        };
        write!(f, "{}", s)
    }
}

/// HTTP/2 ストリーム
#[derive(Debug)]
pub struct Stream {
    /// ストリーム ID
    pub id: u32,
    /// 現在の状態
    pub state: StreamState,
    /// 送信ウィンドウサイズ
    pub send_window: i32,
    /// 受信ウィンドウサイズ
    pub recv_window: i32,
    /// リクエストヘッダー
    pub request_headers: Vec<HeaderField>,
    /// リクエストボディ (累積)
    pub request_body: Vec<u8>,
    /// レスポンスヘッダー
    pub response_headers: Vec<HeaderField>,
    /// レスポンスボディ (累積)
    pub response_body: Vec<u8>,
    /// 依存ストリーム ID
    pub dependency: u32,
    /// 重み (1-256)
    pub weight: u8,
    /// 排他フラグ
    pub exclusive: bool,
    /// ヘッダーブロックフラグメント (CONTINUATION 用)
    pending_headers: Vec<u8>,
    /// HEADERS/CONTINUATION 受信中かどうか
    receiving_headers: bool,
    /// コンテンツ長 (Content-Length ヘッダーがあれば)
    pub content_length: Option<u64>,
    /// 受信済みボディサイズ
    pub received_body_size: u64,
}

impl Stream {
    /// 新しいストリームを作成
    pub fn new(id: u32, initial_window_size: i32) -> Self {
        Self {
            id,
            state: StreamState::Idle,
            send_window: initial_window_size,
            recv_window: initial_window_size,
            request_headers: Vec::new(),
            request_body: Vec::new(),
            response_headers: Vec::new(),
            response_body: Vec::new(),
            dependency: 0,
            weight: 16,
            exclusive: false,
            pending_headers: Vec::new(),
            receiving_headers: false,
            content_length: None,
            received_body_size: 0,
        }
    }

    /// HEADERS 受信
    pub fn recv_headers(&mut self, end_stream: bool) -> Result<(), Http2Error> {
        match self.state {
            StreamState::Idle => {
                self.state = if end_stream {
                    StreamState::HalfClosedRemote
                } else {
                    StreamState::Open
                };
                Ok(())
            }
            StreamState::ReservedRemote => {
                self.state = if end_stream {
                    StreamState::Closed
                } else {
                    StreamState::HalfClosedLocal
                };
                Ok(())
            }
            _ => Err(Http2Error::stream_error(
                self.id,
                Http2ErrorCode::StreamClosed,
                format!("Cannot receive HEADERS in state {}", self.state),
            )),
        }
    }

    /// HEADERS 送信
    pub fn send_headers(&mut self, end_stream: bool) -> Result<(), Http2Error> {
        match self.state {
            StreamState::Idle => {
                self.state = if end_stream {
                    StreamState::HalfClosedLocal
                } else {
                    StreamState::Open
                };
                Ok(())
            }
            StreamState::ReservedLocal => {
                self.state = if end_stream {
                    StreamState::Closed
                } else {
                    StreamState::HalfClosedRemote
                };
                Ok(())
            }
            StreamState::Open | StreamState::HalfClosedRemote => {
                // TRAILERS の場合
                if end_stream {
                    self.state = match self.state {
                        StreamState::Open => StreamState::HalfClosedLocal,
                        StreamState::HalfClosedRemote => StreamState::Closed,
                        _ => self.state,
                    };
                }
                Ok(())
            }
            _ => Err(Http2Error::stream_error(
                self.id,
                Http2ErrorCode::StreamClosed,
                format!("Cannot send HEADERS in state {}", self.state),
            )),
        }
    }

    /// DATA 受信
    pub fn recv_data(&mut self, data: &[u8], end_stream: bool) -> Result<(), Http2Error> {
        match self.state {
            StreamState::Open | StreamState::HalfClosedLocal => {
                // フロー制御チェック
                let data_len = data.len() as i32;
                if data_len > self.recv_window {
                    return Err(Http2Error::stream_error(
                        self.id,
                        Http2ErrorCode::FlowControlError,
                        "Received data exceeds flow control window",
                    ));
                }

                self.recv_window -= data_len;
                self.request_body.extend_from_slice(data);
                self.received_body_size += data.len() as u64;

                if end_stream {
                    self.state = match self.state {
                        StreamState::Open => StreamState::HalfClosedRemote,
                        StreamState::HalfClosedLocal => StreamState::Closed,
                        _ => self.state,
                    };
                }
                Ok(())
            }
            _ => Err(Http2Error::stream_error(
                self.id,
                Http2ErrorCode::StreamClosed,
                format!("Cannot receive DATA in state {}", self.state),
            )),
        }
    }

    /// DATA 送信準備 (ウィンドウチェック)
    pub fn prepare_send_data(&mut self, len: usize) -> Result<(), Http2Error> {
        let len = len as i32;
        if len > self.send_window {
            return Err(Http2Error::stream_error(
                self.id,
                Http2ErrorCode::FlowControlError,
                "Send data exceeds flow control window",
            ));
        }
        self.send_window -= len;
        Ok(())
    }

    /// END_STREAM を送信
    pub fn send_end_stream(&mut self) -> Result<(), Http2Error> {
        match self.state {
            StreamState::Open => {
                self.state = StreamState::HalfClosedLocal;
                Ok(())
            }
            StreamState::HalfClosedRemote => {
                self.state = StreamState::Closed;
                Ok(())
            }
            _ => Err(Http2Error::stream_error(
                self.id,
                Http2ErrorCode::StreamClosed,
                format!("Cannot send END_STREAM in state {}", self.state),
            )),
        }
    }

    /// RST_STREAM 受信
    pub fn recv_rst_stream(&mut self, _error_code: u32) {
        self.state = StreamState::Closed;
    }

    /// RST_STREAM 送信
    pub fn send_rst_stream(&mut self) {
        self.state = StreamState::Closed;
    }

    /// WINDOW_UPDATE 受信 (送信ウィンドウ増加)
    pub fn recv_window_update(&mut self, increment: u32) -> Result<(), Http2Error> {
        let new_window = self.send_window.checked_add(increment as i32);
        match new_window {
            Some(w) if (w as i64) <= 0x7FFFFFFF => {
                self.send_window = w;
                Ok(())
            }
            _ => Err(Http2Error::stream_error(
                self.id,
                Http2ErrorCode::FlowControlError,
                "Window size overflow",
            )),
        }
    }

    /// 受信ウィンドウを更新
    pub fn update_recv_window(&mut self, increment: i32) {
        self.recv_window = self.recv_window.saturating_add(increment);
    }

    /// ヘッダーブロックフラグメントを追加
    pub fn append_header_fragment(&mut self, fragment: &[u8], end_headers: bool) {
        self.pending_headers.extend_from_slice(fragment);
        self.receiving_headers = !end_headers;
    }

    /// ヘッダーブロックを取得してクリア
    pub fn take_header_block(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending_headers)
    }

    /// ヘッダー受信中かどうか
    pub fn is_receiving_headers(&self) -> bool {
        self.receiving_headers
    }

    /// ストリームがアクティブかどうか
    pub fn is_active(&self) -> bool {
        !matches!(self.state, StreamState::Closed | StreamState::Idle)
    }

    /// リクエストメソッドを取得
    pub fn method(&self) -> Option<&[u8]> {
        self.request_headers
            .iter()
            .find(|h| h.name == b":method")
            .map(|h| h.value.as_slice())
    }

    /// リクエストパスを取得
    pub fn path(&self) -> Option<&[u8]> {
        self.request_headers
            .iter()
            .find(|h| h.name == b":path")
            .map(|h| h.value.as_slice())
    }

    /// Authority を取得
    pub fn authority(&self) -> Option<&[u8]> {
        self.request_headers
            .iter()
            .find(|h| h.name == b":authority")
            .map(|h| h.value.as_slice())
    }

    /// Scheme を取得
    pub fn scheme(&self) -> Option<&[u8]> {
        self.request_headers
            .iter()
            .find(|h| h.name == b":scheme")
            .map(|h| h.value.as_slice())
    }
}

/// ストリームマネージャー
pub struct StreamManager {
    /// ストリーム (ID -> Stream)
    streams: HashMap<u32, Stream>,
    /// 次のサーバー発行ストリーム ID (偶数)
    next_server_stream_id: u32,
    /// クライアントから受信した最大ストリーム ID
    max_client_stream_id: u32,
    /// 最大同時ストリーム数
    max_concurrent_streams: u32,
    /// 初期ウィンドウサイズ
    initial_window_size: i32,
    /// ヘッダー受信中のストリーム ID (CONTINUATION 用)
    receiving_headers_stream: Option<u32>,
    /// GOAWAY で受信した last_stream_id (RFC 7540 Section 6.8)
    goaway_last_stream_id: Option<u32>,
}

impl StreamManager {
    /// 新しいストリームマネージャーを作成
    pub fn new(max_concurrent: u32, initial_window: i32) -> Self {
        Self {
            streams: HashMap::new(),
            next_server_stream_id: 2,
            max_client_stream_id: 0,
            max_concurrent_streams: max_concurrent,
            initial_window_size: initial_window,
            receiving_headers_stream: None,
            goaway_last_stream_id: None,
        }
    }

    /// クライアントストリームを取得または作成
    /// 
    /// RFC 7540 Section 6.8: GOAWAY 受信後は、last_stream_id より大きい
    /// ストリーム ID の開始を拒否する。
    pub fn get_or_create_client_stream(&mut self, id: u32) -> Result<&mut Stream, Http2Error> {
        // クライアントストリームは奇数
        if id % 2 == 0 {
            return Err(Http2Error::protocol_error("Client stream ID must be odd"));
        }

        // GOAWAY 受信後のストリーム ID 制限チェック
        if let Some(goaway_id) = self.goaway_last_stream_id {
            if id > goaway_id {
                return Err(Http2Error::connection_error(
                    Http2ErrorCode::RefusedStream,
                    "Stream ID exceeds GOAWAY last_stream_id",
                ));
            }
        }

        // ストリーム ID は単調増加
        if id > self.max_client_stream_id {
            // 同時ストリーム数チェック
            let active_count = self.active_stream_count();
            if active_count >= self.max_concurrent_streams as usize {
                return Err(Http2Error::connection_error(
                    Http2ErrorCode::RefusedStream,
                    "Too many concurrent streams",
                ));
            }

            self.max_client_stream_id = id;
            self.streams.insert(id, Stream::new(id, self.initial_window_size));
        }

        self.streams.get_mut(&id).ok_or_else(|| {
            Http2Error::stream_error(id, Http2ErrorCode::StreamClosed, "Stream not found")
        })
    }

    /// ストリームを取得
    pub fn get(&mut self, id: u32) -> Option<&mut Stream> {
        self.streams.get_mut(&id)
    }

    /// ストリームを取得 (不変参照)
    pub fn get_ref(&self, id: u32) -> Option<&Stream> {
        self.streams.get(&id)
    }

    /// アクティブなストリーム数を取得
    pub fn active_stream_count(&self) -> usize {
        self.streams
            .values()
            .filter(|s| s.is_active())
            .count()
    }

    /// クローズ済みストリームをクリーンアップ
    pub fn cleanup_closed(&mut self) {
        self.streams.retain(|_, s| s.state != StreamState::Closed);
    }

    /// 初期ウィンドウサイズを更新
    pub fn update_initial_window_size(&mut self, new_size: i32) -> Result<(), Http2Error> {
        let delta = new_size - self.initial_window_size;
        self.initial_window_size = new_size;

        // 既存ストリームのウィンドウを更新
        for stream in self.streams.values_mut() {
            let new_window = stream.send_window.checked_add(delta);
            match new_window {
                Some(w) if (w as i64) <= 0x7FFFFFFF => {
                    stream.send_window = w;
                }
                _ => {
                    return Err(Http2Error::connection_error(
                        Http2ErrorCode::FlowControlError,
                        "Window size overflow after SETTINGS update",
                    ));
                }
            }
        }

        Ok(())
    }

    /// ヘッダー受信中のストリームを設定
    pub fn set_receiving_headers(&mut self, stream_id: Option<u32>) {
        self.receiving_headers_stream = stream_id;
    }

    /// ヘッダー受信中のストリーム ID を取得
    pub fn receiving_headers_stream(&self) -> Option<u32> {
        self.receiving_headers_stream
    }

    /// 次のサーバーストリーム ID を発行
    pub fn next_server_stream_id(&mut self) -> u32 {
        let id = self.next_server_stream_id;
        self.next_server_stream_id += 2;
        id
    }

    /// 全ストリームにイテレート
    pub fn iter(&self) -> impl Iterator<Item = (&u32, &Stream)> {
        self.streams.iter()
    }

    /// 全ストリームに可変イテレート
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&u32, &mut Stream)> {
        self.streams.iter_mut()
    }

    /// 最大クライアントストリーム ID を取得
    pub fn max_client_stream_id(&self) -> u32 {
        self.max_client_stream_id
    }

    /// GOAWAY の last_stream_id を設定
    /// 
    /// RFC 7540 Section 6.8: GOAWAY 受信時に呼び出し、
    /// 以降のストリーム作成を制限する。
    pub fn set_goaway_last_stream_id(&mut self, last_stream_id: u32) {
        self.goaway_last_stream_id = Some(last_stream_id);
    }

    /// 優先度に基づいてストリームIDをソートして返す
    /// 
    /// RFC 7540 Section 5.3: 優先度に基づくスケジューリング
    /// 重みが大きいほど高優先度（より多くのリソースを割り当て）
    /// 
    /// 戻り値:
    /// - ストリームIDのVec（優先度順、重みが大きい順）
    pub fn get_streams_by_priority(&self) -> Vec<u32> {
        let mut streams: Vec<_> = self.streams.iter()
            .filter(|(_, s)| s.state == StreamState::Open || s.state == StreamState::HalfClosedRemote)
            .collect();
        
        // 重みが大きい順にソート（同じ重みなら依存先ストリームIDが小さい順）
        streams.sort_by(|a, b| {
            match b.1.weight.cmp(&a.1.weight) {
                std::cmp::Ordering::Equal => a.1.dependency.cmp(&b.1.dependency),
                other => other,
            }
        });
        
        streams.into_iter().map(|(id, _)| *id).collect()
    }

    /// 指定した依存先を持つストリームのIDリストを取得
    /// 
    /// RFC 7540 Section 5.3.1: 依存関係の処理
    #[allow(dead_code)]
    pub fn get_dependent_streams(&self, parent_id: u32) -> Vec<u32> {
        self.streams.iter()
            .filter(|(_, s)| s.dependency == parent_id)
            .map(|(id, _)| *id)
            .collect()
    }

    /// ストリームの優先度情報を更新
    /// 
    /// RFC 7540 Section 5.3.3: 優先度の再設定
    pub fn update_priority(&mut self, stream_id: u32, dependency: u32, weight: u8, exclusive: bool) -> Result<(), Http2Error> {
        // 自己依存チェック
        if stream_id == dependency {
            return Err(Http2Error::protocol_error("Stream cannot depend on itself"));
        }

        if let Some(stream) = self.streams.get_mut(&stream_id) {
            stream.dependency = dependency;
            stream.weight = weight;
            stream.exclusive = exclusive;
            Ok(())
        } else {
            Err(Http2Error::stream_error(stream_id, Http2ErrorCode::StreamClosed, "Stream not found"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stream_state_transitions() {
        let mut stream = Stream::new(1, 65535);
        assert_eq!(stream.state, StreamState::Idle);

        // HEADERS 受信 (end_stream=false)
        stream.recv_headers(false).unwrap();
        assert_eq!(stream.state, StreamState::Open);

        // DATA 受信 (end_stream=true)
        stream.recv_data(b"Hello", true).unwrap();
        assert_eq!(stream.state, StreamState::HalfClosedRemote);

        // HEADERS 送信 (end_stream=true)
        stream.send_headers(true).unwrap();
        assert_eq!(stream.state, StreamState::Closed);
    }

    #[test]
    fn test_stream_manager() {
        let mut manager = StreamManager::new(100, 65535);

        // ストリーム作成
        {
            let stream = manager.get_or_create_client_stream(1).unwrap();
            stream.recv_headers(false).unwrap();
        }
        assert_eq!(manager.active_stream_count(), 1);

        // 同じストリームを再取得
        {
            let stream = manager.get_or_create_client_stream(1).unwrap();
            assert_eq!(stream.id, 1);
        }

        // 新しいストリーム
        {
            let stream = manager.get_or_create_client_stream(3).unwrap();
            stream.recv_headers(false).unwrap();
        }
        assert_eq!(manager.active_stream_count(), 2);
    }

    #[test]
    fn test_stream_manager_max_concurrent() {
        let mut manager = StreamManager::new(2, 65535);

        // 2ストリーム作成
        manager.get_or_create_client_stream(1).unwrap().recv_headers(false).unwrap();
        manager.get_or_create_client_stream(3).unwrap().recv_headers(false).unwrap();

        // 3つ目は失敗
        let result = manager.get_or_create_client_stream(5);
        assert!(result.is_err());
    }

    #[test]
    fn test_flow_control() {
        let mut stream = Stream::new(1, 100);
        stream.recv_headers(false).unwrap();

        // ウィンドウ内のデータ
        stream.recv_data(&[0u8; 50], false).unwrap();
        assert_eq!(stream.recv_window, 50);

        // ウィンドウ超過
        let result = stream.recv_data(&[0u8; 100], false);
        assert!(result.is_err());
    }

    #[test]
    fn test_goaway_stream_rejection() {
        let mut manager = StreamManager::new(100, 65535);

        // ストリーム1, 3を作成（GOAWAY前）
        manager.get_or_create_client_stream(1).unwrap().recv_headers(false).unwrap();
        manager.get_or_create_client_stream(3).unwrap().recv_headers(false).unwrap();

        // GOAWAY last_stream_id=3 を設定
        manager.set_goaway_last_stream_id(3);

        // ストリーム1は既存で last_stream_id 以下なのでOK
        assert!(manager.get_or_create_client_stream(1).is_ok());

        // ストリーム3は last_stream_id と同じなのでOK
        assert!(manager.get_or_create_client_stream(3).is_ok());

        // ストリーム5は last_stream_id を超えるので拒否
        let result = manager.get_or_create_client_stream(5);
        assert!(result.is_err());
        
        // ストリーム7も拒否
        let result = manager.get_or_create_client_stream(7);
        assert!(result.is_err());
        
        // エラーメッセージを確認
        if let Err(e) = result {
            assert!(e.to_string().contains("GOAWAY"));
        }
    }
}
