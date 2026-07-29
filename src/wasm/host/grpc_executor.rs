//! WASM `proxy_grpc_call` 実行エンジン（F-134）
//!
//! 従来 `proxy_grpc_call`/`proxy_grpc_stream`/`proxy_grpc_send` は pending call を
//! `HttpContext`/グローバルレジストリへ登録するだけで、実際に外向き gRPC 呼び出しを
//! 実行するループが存在しなかった（呼び出し元の Wasm ゲストからは `PROXY_RESULT_OK`
//! が返るため成功したように見えるが、`proxy_on_grpc_receive*`/`proxy_on_grpc_close`
//! が永遠に呼ばれない不適合状態だった）。
//!
//! 本モジュールは `src/wasm/http_executor.rs`（`proxy_http_call` 用）と同じ設計方針で、
//! **WASM tick スレッド**（`src/server.rs::spawn_wasm_tick_thread`、io_uring イベント
//! ループとは別の専用バックグラウンドスレッド）上でブロッキング I/O により gRPC-over-h2c
//! のユーナリー呼び出しを実行する。ホットパス絶対規則（io_uring イベントループを
//! 絶対にブロックしない）は、この実行が hot path（データプレーンの接続受理〜転送）
//! に一切関与しない専用スレッド上でのみ行われることで満たす。
//!
//! HTTP/2 のフレーミング・HPACK 符号化/復号は `crate::http2::frame`/`crate::http2::hpack`
//! （送受信バッファに対する純粋な同期変換関数であり、io_uring 非同期 I/O とは無関係）を
//! そのまま再利用する。TCP I/O のみ `std::net::TcpStream`（ブロッキング）を用いる。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use once_cell::sync::Lazy;

use crate::grpc::framing::GrpcFrameDecoder;
use crate::http2::frame::{FrameDecoder, FrameEncoder, FrameHeader, FrameType};
use crate::http2::hpack::{HpackDecoder, HpackEncoder};
use crate::http2::settings::Http2Settings;

/// tick スレッドが拾って実行する、登録済みの gRPC 呼び出し 1 件分。
///
/// `proxy_grpc_call`（ユーナリー、`messages` は要素 1 個）と、
/// `proxy_grpc_stream` + `proxy_grpc_send(end_of_stream=true)`
/// （クライアントストリーミング、`messages` はゲストが送った全メッセージ）の
/// 両方をこの構造体で表す。**実際に送出されるのはゲストがストリームを
/// half-close した時点でまとめて 1 回**であり、メッセージ単位で
/// リアルタイムに送出する真の双方向ストリーミングではない
/// （詳細・理由は `docs/backlog/features/F-139-wasm-grpc-call-execution.md`）。
#[derive(Debug, Clone)]
pub struct PendingGrpcUnaryCall {
    /// 呼び出し元モジュール名
    pub module_name: String,
    /// `proxy_grpc_call` の call_id、または `proxy_grpc_stream` の stream_id
    pub call_id: u32,
    /// 送信先アップストリーム名（`config.upstream_groups` のキー）
    pub upstream: String,
    /// `/<service>/<method>` パス
    pub path: String,
    /// 初期メタデータ
    pub initial_metadata: Vec<(String, String)>,
    /// 送信するメッセージ（複数可、ストリーミングの蓄積分）
    pub messages: Vec<Vec<u8>>,
    /// タイムアウト（ミリ秒）
    pub timeout_ms: u32,
}

/// tick スレッドが処理するグローバル pending gRPC 呼び出しレジストリ
/// （`persistent_context::GLOBAL_PENDING_CALLS` の gRPC 版）。
static GLOBAL_PENDING_GRPC_CALLS: Lazy<RwLock<Vec<PendingGrpcUnaryCall>>> =
    Lazy::new(|| RwLock::new(Vec::new()));

/// 呼び出しをグローバルレジストリへ登録する（`proxy_grpc_call`/`proxy_grpc_send` から）。
pub fn register_global_pending_grpc_call(call: PendingGrpcUnaryCall) {
    if let Ok(mut registry) = GLOBAL_PENDING_GRPC_CALLS.write() {
        registry.push(call);
    }
}

/// キャンセル済み呼び出しをレジストリから取り除く（`proxy_grpc_cancel`/`proxy_grpc_close` から）。
/// 既に tick スレッドが取り出し済み（実行中/完了済み）の場合は何もしない。
pub fn cancel_global_pending_grpc_call(module_name: &str, call_id: u32) -> bool {
    if let Ok(mut registry) = GLOBAL_PENDING_GRPC_CALLS.write() {
        let before = registry.len();
        registry.retain(|c| !(c.module_name == module_name && c.call_id == call_id));
        registry.len() < before
    } else {
        false
    }
}

/// 全ての pending gRPC 呼び出しを取り出す（tick スレッドから）。
pub fn take_global_pending_grpc_calls() -> Vec<PendingGrpcUnaryCall> {
    if let Ok(mut registry) = GLOBAL_PENDING_GRPC_CALLS.write() {
        std::mem::take(&mut *registry)
    } else {
        Vec::new()
    }
}

/// クライアントコネクションプリフェース (RFC 7540 Section 3.5)
const CONNECTION_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// ユーナリー gRPC 呼び出しの結果
#[derive(Debug, Clone)]
pub struct GrpcUnaryResult {
    /// `grpc-status` トレーラー（0 = OK）
    pub status_code: i32,
    /// `grpc-message` トレーラー
    pub status_message: String,
    /// 応答の初期メタデータ（疑似ヘッダを除く）
    pub initial_metadata: Vec<(String, String)>,
    /// 応答メッセージ本体（gRPC 5 バイトフレーミングを剥がした後の Protobuf バイト列）
    pub message: Vec<u8>,
    /// トレーリングメタデータ（`grpc-status`/`grpc-message` を除く）
    pub trailing_metadata: Vec<(String, String)>,
}

/// gRPC-over-h2c のユーナリー呼び出しを 1 本の使い捨て TCP 接続で実行する。
///
/// `path` は `/<service>/<method>` 形式。`initial_metadata` はゲストが
/// `proxy_grpc_call` に渡したメタデータ（gRPC-Metadata としてヘッダに変換される）。
/// 接続プーリングは行わない（1 呼び出し 1 接続。F-106 の HTTP/2 バックエンド
/// プーリングとは別経路であり、頻繁な WASM 発 gRPC 呼び出しがある場合は
/// 将来プーリング化を検討する余地がある。詳細は
/// `docs/backlog/features/F-139-wasm-grpc-call-execution.md`）。
pub fn execute_grpc_unary_call(
    host: &str,
    port: u16,
    path: &str,
    initial_metadata: &[(String, String)],
    messages: &[Vec<u8>],
    timeout_ms: u32,
) -> Result<GrpcUnaryResult, String> {
    let timeout = Duration::from_millis(timeout_ms.max(1) as u64);
    let deadline = Instant::now() + timeout;

    let addr = format!("{host}:{port}");
    let mut stream = TcpStream::connect_timeout(
        &addr
            .parse()
            .map_err(|e| format!("invalid upstream address '{addr}': {e}"))?,
        timeout,
    )
    .map_err(|e| format!("connect failed: {e}"))?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();

    let settings = Http2Settings::new();
    let frame_encoder = FrameEncoder::new(settings.max_frame_size);
    let mut hpack_encoder = HpackEncoder::new(settings.header_table_size as usize);

    // --- 送信: プリフェース + SETTINGS(空) + HEADERS + DATA(END_STREAM) ---
    let mut out = Vec::new();
    out.extend_from_slice(CONNECTION_PREFACE);
    out.extend_from_slice(&frame_encoder.encode_settings(&[], false));

    let authority = format!("{host}:{port}");
    let grpc_timeout = format!("{}m", timeout_ms.max(1));
    let mut headers: Vec<(&[u8], &[u8], bool)> = vec![
        (b":method", b"POST", false),
        (b":scheme", b"http", false),
        (b":path", path.as_bytes(), false),
        (b":authority", authority.as_bytes(), false),
        (b"content-type", b"application/grpc", false),
        (b"te", b"trailers", false),
        (b"grpc-timeout", grpc_timeout.as_bytes(), false),
    ];
    for (k, v) in initial_metadata {
        headers.push((k.as_bytes(), v.as_bytes(), false));
    }
    let header_block = hpack_encoder
        .encode(&headers)
        .map_err(|e| format!("hpack encode failed: {e:?}"))?;

    let stream_id = 1u32;
    out.extend_from_slice(&frame_encoder.encode_headers(
        stream_id,
        &header_block,
        false, // end_stream: gRPC ボディが続く
        true,  // end_headers
        None,
    ));

    // gRPC 5 バイトフレーミング（非圧縮）。複数メッセージ（クライアント
    // ストリーミングの蓄積分）はメッセージ単位でフレーミングして連結する。
    let mut framed = Vec::new();
    for message in messages {
        framed.push(0u8);
        framed.extend_from_slice(&(message.len() as u32).to_be_bytes());
        framed.extend_from_slice(message);
    }
    out.extend_from_slice(&frame_encoder.encode_data(stream_id, &framed, true));

    stream
        .write_all(&out)
        .map_err(|e| format!("write failed: {e}"))?;

    // --- 受信: SETTINGS/HEADERS/DATA を END_STREAM まで読む ---
    let frame_decoder = FrameDecoder::new(defaults_max_frame_size());
    let mut hpack_decoder = HpackDecoder::new(settings.header_table_size as usize);
    let mut grpc_decoder = GrpcFrameDecoder::new();

    let mut read_buf: Vec<u8> = Vec::with_capacity(8192);
    let mut initial_metadata_out: Vec<(String, String)> = Vec::new();
    let mut trailing_metadata_out: Vec<(String, String)> = Vec::new();
    let mut got_response_headers = false;
    let mut end_stream_seen = false;
    let mut tmp = [0u8; 8192];

    while !end_stream_seen {
        if Instant::now() >= deadline {
            return Err("timed out waiting for gRPC response".to_string());
        }

        // フレームヘッダ + ペイロードが揃うまで読み込む
        while read_buf.len() < FrameHeader::SIZE {
            let n = stream
                .read(&mut tmp)
                .map_err(|e| format!("read failed: {e}"))?;
            if n == 0 {
                return Err("connection closed before END_STREAM".to_string());
            }
            read_buf.extend_from_slice(&tmp[..n]);
        }

        let header = frame_decoder
            .decode_header(&read_buf[..FrameHeader::SIZE])
            .map_err(|e| format!("invalid frame header: {e}"))?;
        let total_len = FrameHeader::SIZE + header.length as usize;

        while read_buf.len() < total_len {
            let n = stream
                .read(&mut tmp)
                .map_err(|e| format!("read failed: {e}"))?;
            if n == 0 {
                return Err("connection closed mid-frame".to_string());
            }
            read_buf.extend_from_slice(&tmp[..n]);
        }

        let payload = read_buf[FrameHeader::SIZE..total_len].to_vec();
        let frame = frame_decoder
            .decode(&header, &payload)
            .map_err(|e| format!("frame decode failed: {e}"))?;
        read_buf.drain(..total_len);

        match header.get_frame_type() {
            Some(FrameType::Settings) => {
                if !header.is_ack() {
                    stream
                        .write_all(&frame_encoder.encode_settings_ack())
                        .map_err(|e| format!("write settings ack failed: {e}"))?;
                }
            }
            Some(FrameType::WindowUpdate) | Some(FrameType::Ping) => {
                // ユーナリー呼び出しの小メッセージでは無視して問題ない
                // （送信ウィンドウ枯渇は本経路では想定しない小メッセージのみ対象）。
            }
            Some(FrameType::GoAway) => {
                return Err("server sent GOAWAY before response completed".to_string());
            }
            Some(FrameType::RstStream) => {
                return Err("server sent RST_STREAM".to_string());
            }
            Some(FrameType::Headers) => {
                if let crate::http2::frame::Frame::Headers {
                    end_stream,
                    header_block,
                    ..
                } = frame
                {
                    let fields = hpack_decoder
                        .decode(&header_block)
                        .map_err(|e| format!("hpack decode failed: {e:?}"))?;
                    let pairs: Vec<(String, String)> = fields
                        .into_iter()
                        .map(|f| {
                            (
                                String::from_utf8_lossy(&f.name).to_string(),
                                String::from_utf8_lossy(&f.value).to_string(),
                            )
                        })
                        .collect();

                    let has_grpc_status = pairs.iter().any(|(k, _)| k == "grpc-status");

                    if !got_response_headers && !has_grpc_status {
                        // 通常の応答ヘッダ（:status 等）。疑似ヘッダは除いて
                        // GrpcReceiveInitialMetadata へ渡す。
                        initial_metadata_out =
                            pairs.into_iter().filter(|(k, _)| !k.starts_with(':')).collect();
                        got_response_headers = true;
                    } else {
                        // トレーラー（grpc-status を含む HEADERS、または
                        // trailers-only 応答で最初から grpc-status を含む場合）。
                        trailing_metadata_out = pairs
                            .into_iter()
                            .filter(|(k, _)| !k.starts_with(':'))
                            .collect();
                    }

                    if end_stream {
                        end_stream_seen = true;
                    }
                }
            }
            Some(FrameType::Data) => {
                if let crate::http2::frame::Frame::Data {
                    end_stream, data, ..
                } = frame
                {
                    grpc_decoder.push(&data);
                    if end_stream {
                        end_stream_seen = true;
                    }
                }
            }
            _ => {}
        }
    }

    let message = match grpc_decoder
        .decode_next()
        .map_err(|e| format!("grpc frame decode failed: {e}"))?
    {
        Some(frame) => frame.data,
        None => Vec::new(),
    };

    let status_code: i32 = trailing_metadata_out
        .iter()
        .find(|(k, _)| k == "grpc-status")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let status_message = trailing_metadata_out
        .iter()
        .find(|(k, _)| k == "grpc-message")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    trailing_metadata_out.retain(|(k, _)| k != "grpc-status" && k != "grpc-message");

    Ok(GrpcUnaryResult {
        status_code,
        status_message,
        initial_metadata: initial_metadata_out,
        message,
        trailing_metadata: trailing_metadata_out,
    })
}

/// 受信フレームデコーダの最大フレームサイズ（RFC 7540 既定値 16KiB）。
fn defaults_max_frame_size() -> u32 {
    crate::http2::settings::defaults::MAX_FRAME_SIZE
}

#[cfg(test)]
mod tests {
    use super::*;

    /// アドレス解決に失敗する呼び出しは Err を返す（panic しない）。
    #[test]
    fn test_execute_grpc_unary_call_invalid_address() {
        let result = execute_grpc_unary_call(
            "not a valid host!!",
            0,
            "/test.Service/Method",
            &[],
            &[],
            100,
        );
        assert!(result.is_err());
    }

    /// 接続できないポートへの呼び出しはタイムアウト内に Err を返す。
    #[test]
    fn test_execute_grpc_unary_call_connection_refused() {
        // ポート 0 への接続はプラットフォーム上ほぼ確実に失敗する。
        let result =
            execute_grpc_unary_call("127.0.0.1", 1, "/test.Service/Method", &[], &[], 200);
        assert!(result.is_err());
    }

    /// グローバル pending レジストリの登録・キャンセル・取り出しが機能すること。
    #[test]
    fn test_pending_grpc_call_registry_roundtrip() {
        let call = PendingGrpcUnaryCall {
            module_name: "test_module_grpc_exec".to_string(),
            call_id: 42,
            upstream: "backend".to_string(),
            path: "/test.Service/Method".to_string(),
            initial_metadata: vec![],
            messages: vec![b"hello".to_vec()],
            timeout_ms: 1000,
        };
        register_global_pending_grpc_call(call.clone());

        // 別 call_id は影響を受けない
        assert!(!cancel_global_pending_grpc_call(
            "test_module_grpc_exec",
            9999
        ));
        assert!(cancel_global_pending_grpc_call(
            "test_module_grpc_exec",
            42
        ));

        // 既にキャンセル済みなので取り出しても空
        let taken = take_global_pending_grpc_calls();
        assert!(!taken
            .iter()
            .any(|c| c.module_name == "test_module_grpc_exec" && c.call_id == 42));
    }
}
