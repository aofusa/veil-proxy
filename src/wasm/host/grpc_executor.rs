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
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use once_cell::sync::Lazy;

use super::grpc::GrpcMetadataBlob;
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
    /// 初期メタデータ（F-160: 直列化バイト列のまま保持し、ペアごとの
    /// `String` 確保を発生させない）
    pub initial_metadata: GrpcMetadataBlob,
    /// 送信するメッセージ（複数可、ストリーミングの蓄積分。F-160: `Bytes` で
    /// 参照カウント共有し、蓄積分の一括送出時のディープコピーを避ける）
    pub messages: Vec<Bytes>,
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

/// gRPC-over-h2/h2c 呼び出し用の TCP ストリーム抽象。
///
/// F-134 フォローアップ: 従来は平文 `TcpStream` 決め打ちで gRPC over TLS（h2, ALPN
/// `h2`）上流に接続できなかった（`docs/backlog/features/F-139-wasm-grpc-call-execution.md`
/// の「TLS 上流」課題）。`src/wasm/http_executor.rs::execute_https_request` と同じ
/// 設計（rustls + webpki-roots のシステムルート、ブロッキング I/O）を踏襲し、
/// `Read`/`Write` を実装した列挙体で呼び出し側のロジック（フレーム送受信ループ）を
/// TLS の有無に関わらず共通化する。
enum ClientStream {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Read for ClientStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            ClientStream::Plain(s) => s.read(buf),
            ClientStream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for ClientStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            ClientStream::Plain(s) => s.write(buf),
            ClientStream::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            ClientStream::Plain(s) => s.flush(),
            ClientStream::Tls(s) => s.flush(),
        }
    }
}

/// TLS 上流用の rustls `ClientConnection` を確立し、`TcpStream` を包む。
///
/// システムルート（webpki-roots）で検証する。クライアント証明書認証は
/// veil の他の WASM 発呼経路（`proxy_http_call`）と同様に未対応（相互 TLS が
/// 必要な上流は現状スコープ外、必要になれば `[[upstream]]` 設定に証明書パスを
/// 追加する形で拡張できる）。
fn wrap_tls(stream: TcpStream, host: &str) -> Result<ClientStream, String> {
    use rustls::pki_types::ServerName;
    use rustls::ClientConfig;

    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let mut config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    // gRPC over TLS は ALPN `h2` を要求するサーバーが多い（h2c 相当のフレーミングを
    // そのまま流用するため、ALPN のネゴシエーション結果自体は使わないが、
    // アナウンスしないと拒否するサーバーがある）。
    config.alpn_protocols = vec![b"h2".to_vec()];

    let server_name = ServerName::try_from(host.to_string())
        .map_err(|_| format!("invalid server name: {host}"))?;

    let conn = rustls::ClientConnection::new(Arc::new(config), server_name)
        .map_err(|e| format!("TLS handshake setup failed: {e}"))?;

    Ok(ClientStream::Tls(Box::new(rustls::StreamOwned::new(
        conn, stream,
    ))))
}

/// ユーナリー gRPC 呼び出しの結果
#[derive(Debug, Clone)]
pub struct GrpcUnaryResult {
    /// `grpc-status` トレーラー（0 = OK）
    pub status_code: i32,
    /// `grpc-message` トレーラー
    pub status_message: String,
    /// 応答の初期メタデータ（疑似ヘッダを除く。F-160: `Bytes` で保持）
    pub initial_metadata: Vec<(Bytes, Bytes)>,
    /// 応答メッセージ本体（gRPC 5 バイトフレーミングを剥がした後の Protobuf バイト列）
    pub message: Bytes,
    /// トレーリングメタデータ（`grpc-status`/`grpc-message` を除く。F-160: `Bytes` で保持）
    pub trailing_metadata: Vec<(Bytes, Bytes)>,
}

/// gRPC-over-h2c/h2 のユーナリー呼び出しを 1 本の使い捨て TCP 接続で実行する。
///
/// `path` は `/<service>/<method>` 形式。`initial_metadata` はゲストが
/// `proxy_grpc_call` に渡したメタデータ（gRPC-Metadata としてヘッダに変換される）。
/// `use_tls` が true の場合、`src/wasm/http_executor.rs::execute_https_request` と
/// 同じ rustls + webpki-roots システムルート検証で TLS 上流に接続する（F-134 の
/// 「TLS 上流未対応」課題を解消。詳細は
/// `docs/backlog/features/F-139-wasm-grpc-call-execution.md`）。
/// 接続プーリングは行わない（1 呼び出し 1 接続。F-106 の HTTP/2 バックエンド
/// プーリングとは別経路であり、頻繁な WASM 発 gRPC 呼び出しがある場合は
/// 将来プーリング化を検討する余地がある。詳細は同チケット）。
pub fn execute_grpc_unary_call(
    host: &str,
    port: u16,
    use_tls: bool,
    path: &str,
    initial_metadata: &GrpcMetadataBlob,
    messages: &[Bytes],
    timeout_ms: u32,
) -> Result<GrpcUnaryResult, String> {
    let timeout = Duration::from_millis(timeout_ms.max(1) as u64);
    let deadline = Instant::now() + timeout;

    let addr = format!("{host}:{port}");
    let tcp_stream = TcpStream::connect_timeout(
        &addr
            .parse()
            .map_err(|e| format!("invalid upstream address '{addr}': {e}"))?,
        timeout,
    )
    .map_err(|e| format!("connect failed: {e}"))?;
    tcp_stream.set_read_timeout(Some(timeout)).ok();
    tcp_stream.set_write_timeout(Some(timeout)).ok();

    let mut stream = if use_tls {
        wrap_tls(tcp_stream, host)?
    } else {
        ClientStream::Plain(tcp_stream)
    };

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
    // F-160: 直列化バイト列をコピーせず走査し、そのまま HPACK エンコーダへ渡す。
    for (k, v) in initial_metadata.iter() {
        headers.push((k, v, false));
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
    let mut initial_metadata_out: Vec<(Bytes, Bytes)> = Vec::new();
    let mut trailing_metadata_out: Vec<(Bytes, Bytes)> = Vec::new();
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
                    // F-160: HPACK デコード結果（`f.name`/`f.value`、いずれも既に
                    // 所有された `Vec<u8>`）を `String` へ再変換せず、`Bytes::from`
                    // でそのまま引き継ぐ（UTF-8 検証もコピーも発生しない）。
                    let pairs: Vec<(Bytes, Bytes)> = fields
                        .into_iter()
                        .map(|f| (Bytes::from(f.name), Bytes::from(f.value)))
                        .collect();

                    let has_grpc_status = pairs.iter().any(|(k, _)| k.as_ref() == b"grpc-status");

                    if !got_response_headers && !has_grpc_status {
                        // 通常の応答ヘッダ（:status 等）。疑似ヘッダは除いて
                        // GrpcReceiveInitialMetadata へ渡す。
                        initial_metadata_out = pairs
                            .into_iter()
                            .filter(|(k, _)| !k.starts_with(b":"))
                            .collect();
                        got_response_headers = true;
                    } else {
                        // トレーラー（grpc-status を含む HEADERS、または
                        // trailers-only 応答で最初から grpc-status を含む場合）。
                        trailing_metadata_out = pairs
                            .into_iter()
                            .filter(|(k, _)| !k.starts_with(b":"))
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

    // F-160: `Vec<u8>` を再アロケーションせず `Bytes::from` でそのまま引き継ぐ。
    let message: Bytes = match grpc_decoder
        .decode_next()
        .map_err(|e| format!("grpc frame decode failed: {e}"))?
    {
        Some(frame) => Bytes::from(frame.data),
        None => Bytes::new(),
    };

    let status_code: i32 = trailing_metadata_out
        .iter()
        .find(|(k, _)| k.as_ref() == b"grpc-status")
        .and_then(|(_, v)| std::str::from_utf8(v).ok())
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let status_message = trailing_metadata_out
        .iter()
        .find(|(k, _)| k.as_ref() == b"grpc-message")
        .map(|(_, v)| String::from_utf8_lossy(v).to_string())
        .unwrap_or_default();
    trailing_metadata_out
        .retain(|(k, _)| k.as_ref() != b"grpc-status" && k.as_ref() != b"grpc-message");

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
            false,
            "/test.Service/Method",
            &GrpcMetadataBlob::empty(),
            &[],
            100,
        );
        assert!(result.is_err());
    }

    /// 接続できないポートへの呼び出しはタイムアウト内に Err を返す。
    #[test]
    fn test_execute_grpc_unary_call_connection_refused() {
        // ポート 0 への接続はプラットフォーム上ほぼ確実に失敗する。
        let result = execute_grpc_unary_call(
            "127.0.0.1",
            1,
            false,
            "/test.Service/Method",
            &GrpcMetadataBlob::empty(),
            &[],
            200,
        );
        assert!(result.is_err());
    }

    /// F-134 フォローアップ（TLS 上流対応）: `use_tls=true` でも接続失敗時は
    /// panic せず Err を返す（TLS ハンドシェイク前に TCP connect が失敗する経路）。
    #[test]
    fn test_execute_grpc_unary_call_tls_connection_refused() {
        let result = execute_grpc_unary_call(
            "127.0.0.1",
            1,
            true,
            "/test.Service/Method",
            &GrpcMetadataBlob::empty(),
            &[],
            200,
        );
        assert!(result.is_err());
    }

    /// F-134 フォローアップ: 不正なサーバー名（TLS SNI 用）は TCP 接続前に
    /// 弾かれず、`wrap_tls` の `ServerName::try_from` で BadArgument 相当の
    /// エラーとして trap せず Err になることを確認する
    /// （host 文字列は WASM ゲスト由来であり不正値が来ても panic してはならない）。
    #[test]
    fn test_wrap_tls_invalid_server_name() {
        // rustls はプロセス全体で 1 度だけ CryptoProvider をインストールする必要がある
        // （通常は entry.rs の起動処理が行うが、単体テストバイナリでは未実行）。
        // 複数テストが並行実行されても二重インストールで panic しないよう結果を無視する。
        let _ = rustls::crypto::CryptoProvider::install_default(
            crate::tls_provider::provider::default_provider(),
        );

        // localhost の適当な TCP リスナーを 1 本立てて connect 自体は成功させ、
        // TLS ラップの ServerName 検証だけを単独でテストする。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        // 理由付き allow: 単体テスト（コールドパス、専用テストスレッド）内のみの
        // 同期 connect。本体のホットパス（io_uring イベントループ）には無関係。
        #[allow(clippy::disallowed_methods)]
        let stream = TcpStream::connect(addr).expect("connect should succeed");
        drop(listener);

        let result = wrap_tls(stream, "not a valid server name!!");
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
            initial_metadata: GrpcMetadataBlob::empty(),
            messages: vec![Bytes::from_static(b"hello")],
            timeout_ms: 1000,
        };
        register_global_pending_grpc_call(call.clone());

        // 別 call_id は影響を受けない
        assert!(!cancel_global_pending_grpc_call(
            "test_module_grpc_exec",
            9999
        ));
        assert!(cancel_global_pending_grpc_call("test_module_grpc_exec", 42));

        // 既にキャンセル済みなので取り出しても空
        let taken = take_global_pending_grpc_calls();
        assert!(!taken
            .iter()
            .any(|c| c.module_name == "test_module_grpc_exec" && c.call_id == 42));
    }
}
