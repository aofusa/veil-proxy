//! WASM `proxy_grpc_call` 実行エンジン（F-134 / F-139）
//!
//! F-134 では `proxy_grpc_call`/`proxy_grpc_stream`/`proxy_grpc_send` は pending call を
//! `HttpContext`/グローバルレジストリへ登録するだけで、実際に外向き gRPC 呼び出しを
//! 実行するループが tick スレッド側に実装された。しかし 1 呼び出しはユーナリー完結
//! （ブロッキング）で実行され、100ms 周期の tick スレッド上で他の呼び出しをブロックし、
//! クライアントストリーミングも half-close 時にまとめて 1 回送出するだけだった。
//!
//! F-139 では、
//! 1. 専用の gRPC 実行スレッド（`src/server.rs::spawn_wasm_grpc_thread`、tick スレッドとは
//!    別）を新設し、
//! 2. 上流 (host, port, tls) ごとに HTTP/2 接続を再利用する接続プール（`grpc_pool.rs`）を導入し、
//! 3. 1 呼び出しの完了を待たずに複数呼び出しを並行して 1 ステップずつ進めるノンブロッキング
//!    状態機械（`GrpcRunner`/`ActiveCall`）に置き換える。
//!
//! `execute_grpc_unary_call`（1 呼び出し 1 接続のブロッキング実装）は単体テスト・後方互換の
//! ための**同期フォールバック**として残す。プロダクション経路（`spawn_wasm_grpc_thread`）は
//! 新しい `GrpcRunner` のみを使う。
//!
//! HTTP/2 のフレーミング・HPACK 符号化/復号は `crate::http2::frame`/`crate::http2::hpack`
//! （送受信バッファに対する純粋な同期変換関数であり、io_uring 非同期 I/O とは無関係）を
//! そのまま再利用する。TCP I/O は `std::net::TcpStream` を用いる。
//!
//! **ホットパス絶対規則との関係**: 本モジュールの処理は、データプレーン（io_uring
//! イベントループ）とは完全に別のバックグラウンド専用スレッド上でのみ実行される。
//! したがってブロッキング `poll(2)` / 同期 I/O を使ってよい（`#[allow(clippy::disallowed_methods)]`
//! は理由コメント付きで最小限に）。
//!
//! ## 設計からの意図的な逸脱
//!
//! 設計書 (`docs/artifacts/f139_wasm_grpc_nonblocking_design.md`) の `CallPhase::Connecting` は
//! 本来ノンブロッキング connect（`EINPROGRESS` + `poll(2)` での完了待ち）を想定しているが、
//! 本実装では **接続プールがミスした場合の新規 TCP connect / TLS ハンドシェイクのみ**
//! `GrpcRunner::start_call` 内で同期的（ブロッキング）に行う。理由:
//! - 新規接続はキャッシュミス時（初回・アイドルタイムアウト後）のみ発生し、接続プールに
//!   よってその頻度は大幅に下がる（F-106 と同じ効果）。
//! - 標準ライブラリの `TcpStream` でノンブロッキング connect（`EINPROGRESS` 検出）を安全に
//!   行うには生ソケット操作（`libc::socket`/`connect`/`sockaddr` 手組み）が必要になり、
//!   IPv4/IPv6 双方の正確性を担保するコストが本チケットの他の目標（逐次双方向ストリーミング・
//!   接続プーリング・複数呼び出しの並行進行）に比べてリスクに見合わないと判断した。
//! - 接続確立後の実際のフレーム送受信（`ActiveCall::poll_once`）は完全にノンブロッキングで、
//!   複数呼び出しが 1 呼び出しの送受信待ちで他をブロックすることはない（目標 1〜3 は達成）。
//!
//! この逸脱により、接続プールミス時のみ、その 1 呼び出し分の TCP connect + TLS
//! ハンドシェイクの間だけ gRPC 実行スレッドが止まる（他の呼び出しの進行がその間だけ遅れる）。
//! データプレーンには一切影響しない。

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use once_cell::sync::Lazy;

use super::grpc::GrpcMetadataBlob;
use super::grpc_pool::{ConnKey, GrpcConnPool, PooledConn};
use super::grpc_tls::{wrap_tls, ClientStream, CONNECTION_PREFACE};
use crate::config::UpstreamGroup;
use crate::grpc::framing::GrpcFrameDecoder;
use crate::http2::frame::{Frame, FrameDecoder, FrameEncoder, FrameHeader, FrameType};
use crate::wasm::grpc_status;
use crate::wasm::FilterEngine;

/// tick スレッド（F-134 当時）が拾って実行していた、登録済みユーナリー gRPC 呼び出し
/// 1 件分。F-139 以降は `GrpcRunner::ingest` が消費する（`proxy_grpc_call` 用。
/// `proxy_grpc_stream` + `proxy_grpc_send` はメッセージ単位の `PendingGrpcSend` を使う）。
#[derive(Debug, Clone)]
pub struct PendingGrpcUnaryCall {
    /// 呼び出し元モジュール名
    pub module_name: String,
    /// `proxy_grpc_call` の call_id
    pub call_id: u32,
    /// 送信先アップストリーム名（`config.upstream_groups` のキー）
    pub upstream: String,
    /// `/<service>/<method>` パス
    pub path: String,
    /// 初期メタデータ（F-160: 直列化バイト列のまま保持し、ペアごとの
    /// `String` 確保を発生させない）
    pub initial_metadata: GrpcMetadataBlob,
    /// 送信するメッセージ（ユーナリーなので要素は 1 個）
    pub messages: Vec<Bytes>,
    /// タイムアウト（ミリ秒）
    pub timeout_ms: u32,
}

/// `proxy_grpc_send` が登録する、gRPC ストリーム 1 メッセージ分の逐次送出要求（F-139）。
///
/// 従来（F-134）は half-close までメッセージを溜め、まとめて 1 回だけ
/// `PendingGrpcUnaryCall` 相当として登録していた。F-139 では 1 件ずつ即座に登録し、
/// 対応する `ActiveCall` が未作成（`proxy_grpc_stream` 直後の最初の送出）ならここで
/// 初めて作成できるよう、接続確立に必要な情報一式を毎回載せる
/// （文字列 2 個・`Bytes` クローンのみで、いずれも参照カウント増加かページサイズ以下の
/// 小さな確保であり、WASM ホスト呼び出し自体のコストに比べて無視できる）。
#[derive(Debug, Clone)]
pub struct PendingGrpcSend {
    pub module_name: String,
    /// `proxy_grpc_stream` が返した stream_id
    pub call_id: u32,
    pub upstream: String,
    pub path: String,
    pub initial_metadata: GrpcMetadataBlob,
    pub timeout_ms: u32,
    /// 送信するメッセージ本体（`end_of_stream` のみを伝える場合は空）
    pub message: Bytes,
    /// このメッセージを最後にクライアント側を half-close するか
    pub end_of_stream: bool,
}

/// gRPC 実行スレッドが処理するグローバル pending 呼び出しレジストリ
/// （`persistent_context::GLOBAL_PENDING_CALLS` の gRPC 版）。
static GLOBAL_PENDING_GRPC_CALLS: Lazy<RwLock<Vec<PendingGrpcUnaryCall>>> =
    Lazy::new(|| RwLock::new(Vec::new()));

/// `proxy_grpc_send` が積む逐次送出キュー（F-139）。
static GLOBAL_PENDING_GRPC_SENDS: Lazy<RwLock<Vec<PendingGrpcSend>>> =
    Lazy::new(|| RwLock::new(Vec::new()));

/// `proxy_grpc_cancel`/`proxy_grpc_close` からの、実行中呼び出しへのキャンセル要求（F-139）。
/// まだ pending（`GLOBAL_PENDING_GRPC_CALLS`/`GLOBAL_PENDING_GRPC_SENDS`）の場合は
/// そちらから直接取り除けるが、既に `GrpcRunner` が `ActiveCall` 化している場合は
/// この登録経由で RST_STREAM を送らせる。
static GLOBAL_PENDING_GRPC_CANCELS: Lazy<RwLock<Vec<(String, u32)>>> =
    Lazy::new(|| RwLock::new(Vec::new()));

/// gRPC 実行スレッドの待機用条件変数（ビジースピン禁止）。
/// 新規登録（呼び出し開始・送出・キャンセル）のたびに `notify_one()` する。
static GRPC_WAKE: Lazy<(std::sync::Mutex<bool>, std::sync::Condvar)> =
    Lazy::new(|| (std::sync::Mutex::new(false), std::sync::Condvar::new()));

fn wake_grpc_thread() {
    let (lock, cvar) = &*GRPC_WAKE;
    if let Ok(mut has_work) = lock.lock() {
        *has_work = true;
    }
    cvar.notify_one();
}

/// アクティブな呼び出しが無いとき、新規登録があるまで待つ（ビジースピン禁止）。
/// シャットダウンフラグの定期確認のため、待機には上限（1 秒）を設ける。
///
/// 理由付き allow: 専用 gRPC 実行スレッド（データプレーンとは別スレッド）上の
/// 条件変数待ちであり、io_uring イベントループには一切関与しない。
#[allow(clippy::disallowed_methods)]
pub fn wait_for_grpc_work() {
    let (lock, cvar) = &*GRPC_WAKE;
    if let Ok(mut has_work) = lock.lock() {
        while !*has_work {
            let (guard, result) = cvar
                .wait_timeout(has_work, Duration::from_secs(1))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            has_work = guard;
            if result.timed_out() {
                // シャットダウンフラグ確認のための定期ウェイクアップ。呼び出し側が
                // ループの先頭で SHUTDOWN_FLAG を確認するので、ここでは単に戻る。
                break;
            }
        }
        *has_work = false;
    }
}

/// 呼び出しをグローバルレジストリへ登録する（`proxy_grpc_call` から）。
pub fn register_global_pending_grpc_call(call: PendingGrpcUnaryCall) {
    if let Ok(mut registry) = GLOBAL_PENDING_GRPC_CALLS.write() {
        registry.push(call);
    }
    wake_grpc_thread();
}

/// メッセージ 1 件分の逐次送出要求を登録する（`proxy_grpc_send` から、F-139）。
pub fn register_global_pending_grpc_send(send: PendingGrpcSend) {
    if let Ok(mut registry) = GLOBAL_PENDING_GRPC_SENDS.write() {
        registry.push(send);
    }
    wake_grpc_thread();
}

/// キャンセル済み呼び出しをレジストリから取り除く（`proxy_grpc_cancel`/`proxy_grpc_close` から）。
/// まだ実行スレッドに取り込まれていない pending call/send があればそれを取り除き、
/// 既に `ActiveCall`化されている可能性に備えてキャンセル要求も積んでおく
/// （実行スレッド側で該当が無ければ単に無視される）。
pub fn cancel_global_pending_grpc_call(module_name: &str, call_id: u32) -> bool {
    let mut removed = false;
    if let Ok(mut registry) = GLOBAL_PENDING_GRPC_CALLS.write() {
        let before = registry.len();
        registry.retain(|c| !(c.module_name == module_name && c.call_id == call_id));
        removed |= registry.len() < before;
    }
    if let Ok(mut registry) = GLOBAL_PENDING_GRPC_SENDS.write() {
        let before = registry.len();
        registry.retain(|c| !(c.module_name == module_name && c.call_id == call_id));
        removed |= registry.len() < before;
    }
    if let Ok(mut cancels) = GLOBAL_PENDING_GRPC_CANCELS.write() {
        cancels.push((module_name.to_string(), call_id));
    }
    wake_grpc_thread();
    removed
}

/// 全ての pending gRPC 呼び出しを取り出す（gRPC 実行スレッドから）。
pub fn take_global_pending_grpc_calls() -> Vec<PendingGrpcUnaryCall> {
    if let Ok(mut registry) = GLOBAL_PENDING_GRPC_CALLS.write() {
        std::mem::take(&mut *registry)
    } else {
        Vec::new()
    }
}

/// 全ての pending 送出要求を取り出す（gRPC 実行スレッドから、F-139）。
pub fn take_global_pending_grpc_sends() -> Vec<PendingGrpcSend> {
    if let Ok(mut registry) = GLOBAL_PENDING_GRPC_SENDS.write() {
        std::mem::take(&mut *registry)
    } else {
        Vec::new()
    }
}

/// 全ての pending キャンセル要求を取り出す（gRPC 実行スレッドから、F-139）。
pub fn take_global_pending_grpc_cancels() -> Vec<(String, u32)> {
    if let Ok(mut registry) = GLOBAL_PENDING_GRPC_CANCELS.write() {
        std::mem::take(&mut *registry)
    } else {
        Vec::new()
    }
}

// ============================================================================
// F-139: ノンブロッキング状態機械（GrpcRunner / ActiveCall）
// ============================================================================

/// 実行スレッドがゲストへ橋渡しすべきイベント。
/// 順序は InitialMetadata → Message* → TrailingMetadata → Close を厳守する
/// （`GrpcRunner::poll_all` の呼び出し元が受け取った順に処理すれば自然に守られる）。
#[derive(Debug)]
pub enum GrpcEvent {
    InitialMetadata {
        module_name: String,
        call_id: u32,
        metadata: Vec<(Bytes, Bytes)>,
    },
    Message {
        module_name: String,
        call_id: u32,
        message: Bytes,
    },
    TrailingMetadata {
        module_name: String,
        call_id: u32,
        metadata: Vec<(Bytes, Bytes)>,
    },
    Close {
        module_name: String,
        call_id: u32,
        status_code: i32,
    },
}

/// 呼び出しの進行フェーズ。
///
/// 設計書の `CallPhase` は `Connecting`/`SendingHeaders`/`Streaming`/`Done` の
/// 4 段階だが、「設計からの意図的な逸脱」節のとおり `Connecting`/`SendingHeaders`
/// 相当（接続プールのチェックアウト or 新規 TCP connect/TLS ハンドシェイク、
/// HEADERS の送出）は `GrpcRunner::start_call` 内で同期的に完了させてから
/// `ActiveCall` を作るため、`ActiveCall` として観測可能な状態は `Streaming`/`Done`
/// の 2 つのみになる（未使用の列挙子を残すと dead_code 警告になるため、実際に
/// 観測される状態のみを表現する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallPhase {
    Streaming,
    Done,
}

/// 実行中の gRPC 呼び出し 1 件分の状態。
struct ActiveCall {
    module_name: String,
    call_id: u32,
    key: ConnKey,
    conn: PooledConn,
    stream_id: u32,
    phase: CallPhase,
    /// `proxy_grpc_send` で積まれた未送出メッセージ。
    outbox: VecDeque<Bytes>,
    /// ゲストがこれ以上 send しない（ユーナリーは常に true）。
    end_of_stream_requested: bool,
    /// 実際に END_STREAM 付き DATA フレームを送出済みか。
    half_closed_sent: bool,
    grpc_dec: GrpcFrameDecoder,
    got_initial_metadata: bool,
    /// ストリームレベルの送信フロー制御ウィンドウ。
    stream_send_window: i64,
    deadline: Instant,
}

impl ActiveCall {
    fn raw_fd(&self) -> RawFd {
        match &self.conn.stream {
            ClientStream::Plain(s) => s.as_raw_fd(),
            ClientStream::Tls(s) => s.sock.as_raw_fd(),
        }
    }

    fn has_pending_write(&self) -> bool {
        self.conn.write_offset < self.conn.write_buf.len() || !self.outbox.is_empty()
    }

    fn close_event(&self, status_code: i32) -> GrpcEvent {
        GrpcEvent::Close {
            module_name: self.module_name.clone(),
            call_id: self.call_id,
            status_code,
        }
    }

    /// 1 呼び出しを 1 ステップ進める。`WouldBlock` まで書き、`WouldBlock` まで読み、
    /// 完成したフレーム・メッセージをイベントとして返す。
    fn poll_once(&mut self, encoder: &FrameEncoder, decoder: &FrameDecoder) -> Vec<GrpcEvent> {
        let mut events = Vec::new();

        if self.phase == CallPhase::Done {
            return events;
        }

        if Instant::now() >= self.deadline {
            self.conn.goaway = true;
            events.push(self.close_event(grpc_status::DEADLINE_EXCEEDED));
            self.phase = CallPhase::Done;
            return events;
        }

        // --- 送信: outbox のメッセージをフロー制御に従って write_buf へ積む ---
        while let Some(msg) = self.outbox.front() {
            let framed_len = 5 + msg.len();
            if framed_len as i64 > self.stream_send_window
                || framed_len as i64 > self.conn.conn_send_window
            {
                // ウィンドウ不足。WINDOW_UPDATE を待つ（F-106 の教訓）。
                break;
            }
            let msg = self.outbox.pop_front().expect("front checked above");
            let mut framed = Vec::with_capacity(framed_len);
            framed.push(0u8);
            framed.extend_from_slice(&(msg.len() as u32).to_be_bytes());
            framed.extend_from_slice(&msg);
            let end_stream = self.outbox.is_empty() && self.end_of_stream_requested;
            encoder.encode_data_into(
                &mut self.conn.write_buf,
                self.stream_id,
                &framed,
                end_stream,
            );
            self.stream_send_window -= framed_len as i64;
            self.conn.conn_send_window -= framed_len as i64;
            if end_stream {
                self.half_closed_sent = true;
            }
        }
        if self.outbox.is_empty() && self.end_of_stream_requested && !self.half_closed_sent {
            encoder.encode_data_into(&mut self.conn.write_buf, self.stream_id, &[], true);
            self.half_closed_sent = true;
        }

        // 実ソケットへ書き出す（WouldBlock まで、部分書き込みはオフセットで持ち越す）。
        while self.conn.write_offset < self.conn.write_buf.len() {
            match self
                .conn
                .stream
                .write(&self.conn.write_buf[self.conn.write_offset..])
            {
                Ok(0) => break,
                Ok(n) => self.conn.write_offset += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    ftlog::warn!("[wasm:grpc] write failed: {e}");
                    self.conn.goaway = true;
                    events.push(self.close_event(grpc_status::UNAVAILABLE));
                    self.phase = CallPhase::Done;
                    return events;
                }
            }
        }
        if self.conn.write_offset > 0 && self.conn.write_offset == self.conn.write_buf.len() {
            self.conn.write_buf.clear();
            self.conn.write_offset = 0;
        }

        // --- 受信: WouldBlock までフレームを読み、種別ごとに処理する ---
        let mut tmp = [0u8; 8192];
        loop {
            match self.conn.stream.read(&mut tmp) {
                Ok(0) => {
                    self.conn.goaway = true;
                    events.push(self.close_event(grpc_status::UNAVAILABLE));
                    self.phase = CallPhase::Done;
                    return events;
                }
                Ok(n) => self.conn.read_buf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    ftlog::warn!("[wasm:grpc] read failed: {e}");
                    self.conn.goaway = true;
                    events.push(self.close_event(grpc_status::UNAVAILABLE));
                    self.phase = CallPhase::Done;
                    return events;
                }
            }
        }

        loop {
            match try_take_frame(&mut self.conn.read_buf, decoder) {
                Ok(Some((header, frame))) => {
                    if let Some(true) = self.handle_frame(encoder, &header, frame, &mut events) {
                        self.phase = CallPhase::Done;
                        return events;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    ftlog::warn!("[wasm:grpc] frame decode failed: {e}");
                    self.conn.goaway = true;
                    events.push(self.close_event(grpc_status::INTERNAL));
                    self.phase = CallPhase::Done;
                    return events;
                }
            }
        }

        events
    }

    /// 1 フレームを処理する。呼び出しが完了した場合は `Some(true)` を返す。
    fn handle_frame(
        &mut self,
        encoder: &FrameEncoder,
        header: &FrameHeader,
        frame: Frame,
        events: &mut Vec<GrpcEvent>,
    ) -> Option<bool> {
        match header.get_frame_type() {
            Some(FrameType::Settings) => {
                if !header.is_ack() {
                    if let Frame::Settings { settings, .. } = &frame {
                        for (id, value) in settings {
                            // SETTINGS_INITIAL_WINDOW_SIZE (0x4)
                            if *id == 0x4 {
                                self.conn.peer_initial_window = *value as i64;
                            }
                        }
                    }
                    self.conn
                        .write_buf
                        .extend_from_slice(&encoder.encode_settings_ack());
                }
                None
            }
            Some(FrameType::WindowUpdate) => {
                if let Frame::WindowUpdate {
                    stream_id,
                    increment,
                } = frame
                {
                    if stream_id == 0 {
                        self.conn.conn_send_window += increment as i64;
                    } else if stream_id == self.stream_id {
                        self.stream_send_window += increment as i64;
                    }
                }
                None
            }
            Some(FrameType::Ping) => {
                if let Frame::Ping { ack, data } = frame {
                    if !ack {
                        self.conn
                            .write_buf
                            .extend_from_slice(&encoder.encode_ping(&data, true));
                    }
                }
                None
            }
            Some(FrameType::GoAway) => {
                self.conn.goaway = true;
                events.push(self.close_event(grpc_status::UNAVAILABLE));
                Some(true)
            }
            Some(FrameType::RstStream) => {
                self.conn.goaway = true;
                events.push(self.close_event(grpc_status::CANCELLED));
                Some(true)
            }
            Some(FrameType::Headers) => {
                let Frame::Headers {
                    end_stream,
                    header_block,
                    ..
                } = frame
                else {
                    return None;
                };
                let fields = match self.conn.hpack_dec.decode(&header_block) {
                    Ok(f) => f,
                    Err(e) => {
                        ftlog::warn!("[wasm:grpc] hpack decode failed: {e:?}");
                        self.conn.goaway = true;
                        events.push(self.close_event(grpc_status::INTERNAL));
                        return Some(true);
                    }
                };
                // F-160 と同様に `Bytes::from` でそのまま引き継ぐ（コピー無し）。
                let pairs: Vec<(Bytes, Bytes)> = fields
                    .into_iter()
                    .map(|f| (Bytes::from(f.name), Bytes::from(f.value)))
                    .collect();
                let has_grpc_status = pairs.iter().any(|(k, _)| k.as_ref() == b"grpc-status");

                if !self.got_initial_metadata && !has_grpc_status {
                    self.got_initial_metadata = true;
                    let metadata: Vec<(Bytes, Bytes)> = pairs
                        .into_iter()
                        .filter(|(k, _)| !k.starts_with(b":"))
                        .collect();
                    if !metadata.is_empty() {
                        events.push(GrpcEvent::InitialMetadata {
                            module_name: self.module_name.clone(),
                            call_id: self.call_id,
                            metadata,
                        });
                    }
                    if end_stream {
                        events.push(self.close_event(grpc_status::OK));
                        return Some(true);
                    }
                    None
                } else {
                    // トレーラー（grpc-status を含む HEADERS）。
                    let status_code: i32 = pairs
                        .iter()
                        .find(|(k, _)| k.as_ref() == b"grpc-status")
                        .and_then(|(_, v)| std::str::from_utf8(v).ok())
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(grpc_status::UNKNOWN);
                    let trailing: Vec<(Bytes, Bytes)> = pairs
                        .into_iter()
                        .filter(|(k, _)| {
                            !k.starts_with(b":")
                                && k.as_ref() != b"grpc-status"
                                && k.as_ref() != b"grpc-message"
                        })
                        .collect();
                    if !trailing.is_empty() {
                        events.push(GrpcEvent::TrailingMetadata {
                            module_name: self.module_name.clone(),
                            call_id: self.call_id,
                            metadata: trailing,
                        });
                    }
                    events.push(self.close_event(status_code));
                    Some(true)
                }
            }
            Some(FrameType::Data) => {
                let Frame::Data {
                    end_stream, data, ..
                } = frame
                else {
                    return None;
                };
                self.grpc_dec.push(&data);
                loop {
                    match self.grpc_dec.decode_next() {
                        Ok(Some(f)) => events.push(GrpcEvent::Message {
                            module_name: self.module_name.clone(),
                            call_id: self.call_id,
                            message: Bytes::from(f.data),
                        }),
                        Ok(None) => break,
                        Err(e) => {
                            ftlog::warn!("[wasm:grpc] grpc frame decode failed: {e}");
                            self.conn.goaway = true;
                            events.push(self.close_event(grpc_status::INTERNAL));
                            return Some(true);
                        }
                    }
                }
                if end_stream {
                    events.push(self.close_event(grpc_status::OK));
                    Some(true)
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

/// 受信バッファから完成しているフレームを 1 つ取り出す（無ければ `Ok(None)`）。
fn try_take_frame(
    read_buf: &mut Vec<u8>,
    decoder: &FrameDecoder,
) -> Result<Option<(FrameHeader, Frame)>, String> {
    if read_buf.len() < FrameHeader::SIZE {
        return Ok(None);
    }
    let header = decoder
        .decode_header(&read_buf[..FrameHeader::SIZE])
        .map_err(|e| format!("invalid frame header: {e}"))?;
    let total_len = FrameHeader::SIZE + header.length as usize;
    if read_buf.len() < total_len {
        return Ok(None);
    }
    let payload = read_buf[FrameHeader::SIZE..total_len].to_vec();
    let frame = decoder
        .decode(&header, &payload)
        .map_err(|e| format!("frame decode failed: {e}"))?;
    read_buf.drain(..total_len);
    Ok(Some((header, frame)))
}

/// 専用 gRPC 実行スレッドが保持する状態機械本体。
///
/// 接続プール（`GrpcConnPool`）・アクティブな呼び出し集合の両方をこの構造体だけが
/// 所有し、グローバル static としては公開しない（このスレッドの外から触られない）。
pub struct GrpcRunner {
    pool: GrpcConnPool,
    active: HashMap<(String, u32), ActiveCall>,
    frame_encoder: FrameEncoder,
    frame_decoder: FrameDecoder,
}

impl Default for GrpcRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl GrpcRunner {
    pub fn new() -> Self {
        let max_frame_size = crate::http2::settings::defaults::MAX_FRAME_SIZE;
        Self {
            pool: GrpcConnPool::new(),
            active: HashMap::new(),
            frame_encoder: FrameEncoder::new(max_frame_size),
            frame_decoder: FrameDecoder::new(max_frame_size),
        }
    }

    /// アクティブな呼び出しが 1 件でもあるか（待機方式の切り替えに使う）。
    pub fn has_active_calls(&self) -> bool {
        !self.active.is_empty()
    }

    /// 新規呼び出し・送出・キャンセルを取り込む。
    #[allow(clippy::too_many_arguments)]
    pub fn ingest(
        &mut self,
        new_calls: Vec<PendingGrpcUnaryCall>,
        new_sends: Vec<PendingGrpcSend>,
        cancels: Vec<(String, u32)>,
        upstream_groups: &HashMap<String, Arc<UpstreamGroup>>,
    ) -> Vec<GrpcEvent> {
        let mut events = Vec::new();

        for call in new_calls {
            self.start_call(
                call.module_name,
                call.call_id,
                &call.upstream,
                call.path,
                call.initial_metadata,
                call.timeout_ms,
                call.messages,
                true,
                upstream_groups,
                &mut events,
            );
        }

        for send in new_sends {
            let key = (send.module_name.clone(), send.call_id);
            if let Some(active) = self.active.get_mut(&key) {
                if !send.message.is_empty() {
                    active.outbox.push_back(send.message);
                }
                if send.end_of_stream {
                    active.end_of_stream_requested = true;
                }
            } else {
                let messages = if send.message.is_empty() {
                    Vec::new()
                } else {
                    vec![send.message]
                };
                self.start_call(
                    send.module_name,
                    send.call_id,
                    &send.upstream,
                    send.path,
                    send.initial_metadata,
                    send.timeout_ms,
                    messages,
                    send.end_of_stream,
                    upstream_groups,
                    &mut events,
                );
            }
        }

        for (module_name, call_id) in cancels {
            if let Some(active) = self.active.remove(&(module_name, call_id)) {
                // ベストエフォートで RST_STREAM を送る（送出失敗・部分送信は無視して破棄する）。
                let rst = self
                    .frame_encoder
                    .encode_rst_stream(active.stream_id, 0x8 /* CANCEL */);
                let mut conn = active.conn;
                let _ = conn.stream.write(&rst);
                // 破棄（プールへは戻さない。RST_STREAM 送出後の接続状態は信頼できない
                // ため単純化して閉じる）。
            }
        }

        events
    }

    /// 新規呼び出しを開始する（接続プールのチェックアウト、ミス時は新規接続、
    /// HEADERS の送出まで）。失敗時は `events` へ `Close` を積んで終了する。
    #[allow(clippy::too_many_arguments)]
    fn start_call(
        &mut self,
        module_name: String,
        call_id: u32,
        upstream: &str,
        path: String,
        initial_metadata: GrpcMetadataBlob,
        timeout_ms: u32,
        initial_messages: Vec<Bytes>,
        end_of_stream: bool,
        upstream_groups: &HashMap<String, Arc<UpstreamGroup>>,
        events: &mut Vec<GrpcEvent>,
    ) {
        let Some(group) = upstream_groups.get(upstream) else {
            ftlog::warn!("[wasm:grpc] upstream '{upstream}' not found for module '{module_name}'");
            events.push(GrpcEvent::Close {
                module_name,
                call_id,
                status_code: grpc_status::UNIMPLEMENTED,
            });
            return;
        };
        let Some(server) = group.select("0.0.0.0") else {
            ftlog::warn!("[wasm:grpc] no healthy servers in upstream '{upstream}' for module '{module_name}'");
            events.push(GrpcEvent::Close {
                module_name,
                call_id,
                status_code: grpc_status::UNAVAILABLE,
            });
            return;
        };
        let host = server.host().to_string();
        let port = server.port();
        let use_tls = server.use_tls();
        let key: ConnKey = (host.clone(), port, use_tls);

        // 接続プールをチェックアウトし、ミスした場合のみ新規接続する
        // （設計からの意図的な逸脱: この新規接続はブロッキング。理由はモジュール冒頭コメント参照）。
        let mut conn = match self.pool.checkout(&key) {
            Some(conn) => conn,
            None => match connect_new(&host, port, use_tls, timeout_ms) {
                Ok(conn) => conn,
                Err(e) => {
                    ftlog::warn!("[wasm:grpc] connect to '{host}:{port}' failed: {e}");
                    events.push(GrpcEvent::Close {
                        module_name,
                        call_id,
                        status_code: grpc_status::UNAVAILABLE,
                    });
                    return;
                }
            },
        };

        if !conn.preface_sent {
            conn.write_buf.extend_from_slice(CONNECTION_PREFACE);
            conn.write_buf
                .extend_from_slice(&self.frame_encoder.encode_settings(&[], false));
            conn.preface_sent = true;
        }

        let stream_id = conn.next_stream_id;
        conn.next_stream_id += 2;

        let authority = format!("{host}:{port}");
        let grpc_timeout = format!("{}m", timeout_ms.max(1));
        let scheme: &[u8] = if use_tls { b"https" } else { b"http" };
        let mut headers: Vec<(&[u8], &[u8], bool)> = vec![
            (b":method", b"POST", false),
            (b":scheme", scheme, false),
            (b":path", path.as_bytes(), false),
            (b":authority", authority.as_bytes(), false),
            (b"content-type", b"application/grpc", false),
            (b"te", b"trailers", false),
            (b"grpc-timeout", grpc_timeout.as_bytes(), false),
        ];
        for (k, v) in initial_metadata.iter() {
            headers.push((k, v, false));
        }
        let header_block = match conn.hpack_enc.encode(&headers) {
            Ok(b) => b,
            Err(e) => {
                ftlog::warn!("[wasm:grpc] hpack encode failed: {e:?}");
                events.push(GrpcEvent::Close {
                    module_name,
                    call_id,
                    status_code: grpc_status::INTERNAL,
                });
                return;
            }
        };
        self.frame_encoder.encode_headers_into(
            &mut conn.write_buf,
            stream_id,
            &header_block,
            false,
            true,
            None,
        );

        // 接続プールから取り出した既存接続もこの時点で必ずノンブロッキング化する
        // （新規接続は `connect_new` 内で既に設定済みだが、念のため冪等に呼ぶ）。
        if let Err(e) = conn.stream.set_nonblocking(true) {
            ftlog::warn!("[wasm:grpc] set_nonblocking failed: {e}");
        }

        let stream_send_window = conn.peer_initial_window;

        let active = ActiveCall {
            module_name,
            call_id,
            key,
            conn,
            stream_id,
            phase: CallPhase::Streaming,
            outbox: initial_messages.into_iter().collect(),
            end_of_stream_requested: end_of_stream,
            half_closed_sent: false,
            grpc_dec: GrpcFrameDecoder::new(),
            got_initial_metadata: false,
            stream_send_window,
            deadline: Instant::now() + Duration::from_millis(timeout_ms.max(1) as u64),
        };
        self.active
            .insert((active.module_name.clone(), call_id), active);
    }

    /// アクティブな全呼び出しを 1 ステップ進める。poll(2) で
    /// 「いずれかのソケットが読み書き可能 or 最短デッドライン」まで待つ。
    ///
    /// 理由付き allow: 専用 gRPC 実行スレッド（データプレーンとは別スレッド）上の
    /// ブロッキング `poll(2)` 待機。io_uring イベントループには一切関与しない。
    #[allow(clippy::disallowed_methods)]
    pub fn poll_all(&mut self) -> Vec<GrpcEvent> {
        if self.active.is_empty() {
            return Vec::new();
        }

        let now = Instant::now();
        // 上限（デッドライン監視・シャットダウン検知の保険）。
        let mut timeout_ms: i32 = 1000;
        let mut pollfds: Vec<libc::pollfd> = Vec::with_capacity(self.active.len());
        let mut keys: Vec<(String, u32)> = Vec::with_capacity(self.active.len());
        for (key, call) in self.active.iter() {
            let remaining_ms = call
                .deadline
                .checked_duration_since(now)
                .map(|d| d.as_millis().min(i32::MAX as u128) as i32)
                .unwrap_or(0);
            timeout_ms = timeout_ms.min(remaining_ms);

            let mut ev = libc::POLLIN;
            if call.has_pending_write() {
                ev |= libc::POLLOUT;
            }
            pollfds.push(libc::pollfd {
                fd: call.raw_fd(),
                events: ev,
                revents: 0,
            });
            keys.push(key.clone());
        }

        unsafe {
            libc::poll(
                pollfds.as_mut_ptr(),
                pollfds.len() as libc::nfds_t,
                timeout_ms.max(0),
            );
        }

        let mut events = Vec::new();
        for key in &keys {
            if let Some(call) = self.active.get_mut(key) {
                events.extend(call.poll_once(&self.frame_encoder, &self.frame_decoder));
            }
        }

        let done_keys: Vec<(String, u32)> = self
            .active
            .iter()
            .filter(|(_, c)| c.phase == CallPhase::Done)
            .map(|(k, _)| k.clone())
            .collect();
        for key in done_keys {
            if let Some(call) = self.active.remove(&key) {
                self.pool.checkin(call.key, call.conn);
            }
        }

        self.pool.sweep_idle();

        events
    }
}

/// プールミス時の新規接続（TCP connect + 必要なら TLS ハンドシェイク）。
/// 「設計からの意図的な逸脱」節のとおり同期（ブロッキング）で行う。
fn connect_new(
    host: &str,
    port: u16,
    use_tls: bool,
    timeout_ms: u32,
) -> Result<PooledConn, String> {
    let timeout = Duration::from_millis(timeout_ms.max(1) as u64);
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

    let stream = if use_tls {
        wrap_tls(tcp_stream, host)?
    } else {
        ClientStream::Plain(tcp_stream)
    };
    stream
        .set_nonblocking(true)
        .map_err(|e| format!("set_nonblocking failed: {e}"))?;

    let header_table_size = crate::http2::settings::defaults::HEADER_TABLE_SIZE as usize;
    Ok(PooledConn::new(stream, header_table_size))
}

// ============================================================================
// ゲストへのイベント配送
// ============================================================================

/// `GrpcRunner` が生成したイベントを対応する `proxy_on_grpc_receive*` コールバックへ
/// 橋渡しする。呼び出し元（gRPC 実行スレッド）が `Vec<GrpcEvent>` を受け取った順に
/// 1 件ずつこの関数を呼べば、InitialMetadata → Message* → TrailingMetadata → Close の
/// 順序は自然に保たれる。
pub fn deliver_event(engine: &Arc<FilterEngine>, event: GrpcEvent) {
    match event {
        GrpcEvent::InitialMetadata {
            module_name,
            call_id,
            metadata,
        } => {
            crate::wasm::grpc_integration::on_grpc_initial_metadata(
                engine,
                &module_name,
                call_id,
                &metadata,
            );
        }
        GrpcEvent::Message {
            module_name,
            call_id,
            message,
        } => {
            crate::wasm::grpc_integration::on_grpc_message(engine, &module_name, call_id, &message);
        }
        GrpcEvent::TrailingMetadata {
            module_name,
            call_id,
            metadata,
        } => {
            crate::wasm::grpc_integration::on_grpc_trailing_metadata(
                engine,
                &module_name,
                call_id,
                &metadata,
            );
        }
        GrpcEvent::Close {
            module_name,
            call_id,
            status_code,
        } => {
            crate::wasm::grpc_integration::on_grpc_close(
                engine,
                &module_name,
                call_id,
                status_code,
            );
        }
    }
}

// ============================================================================
// 同期フォールバック（単体テスト・後方互換専用）
// ============================================================================

/// ユーナリー gRPC 呼び出しの結果
///
/// `execute_grpc_unary_call`（同期フォールバック）専用の戻り値型。プロダクション経路
/// （`GrpcRunner`）はこの型を経由しないため、テストビルドでのみコンパイルする
/// （`cfg(test)` を外すと非テストビルドで dead_code 警告になる）。
#[cfg(test)]
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

/// gRPC-over-h2c/h2 のユーナリー呼び出しを 1 本の使い捨て TCP 接続で実行する
/// **同期フォールバック**。
///
/// F-139 でプロダクション経路は `GrpcRunner`（本ファイル上部の状態機械）に一本化された。
/// この関数は単体テスト・後方互換のためだけに残っている（1 呼び出し 1 接続・
/// ブロッキング・接続プールなし。テスト以外では呼び出さないこと）。
/// プロダクションコードから呼ばれなくなったため `cfg(test)` を付けている
/// （外すと非テストビルドで dead_code 警告になる）。
#[cfg(test)]
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

    let settings = crate::http2::settings::Http2Settings::new();
    let frame_encoder = FrameEncoder::new(settings.max_frame_size);
    let mut hpack_encoder =
        crate::http2::hpack::HpackEncoder::new(settings.header_table_size as usize);

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
        false,
        true,
        None,
    ));

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
    let mut hpack_decoder =
        crate::http2::hpack::HpackDecoder::new(settings.header_table_size as usize);
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
                // ユーナリー呼び出しの小メッセージでは無視して問題ない。
            }
            Some(FrameType::GoAway) => {
                return Err("server sent GOAWAY before response completed".to_string());
            }
            Some(FrameType::RstStream) => {
                return Err("server sent RST_STREAM".to_string());
            }
            Some(FrameType::Headers) => {
                if let Frame::Headers {
                    end_stream,
                    header_block,
                    ..
                } = frame
                {
                    let fields = hpack_decoder
                        .decode(&header_block)
                        .map_err(|e| format!("hpack decode failed: {e:?}"))?;
                    let pairs: Vec<(Bytes, Bytes)> = fields
                        .into_iter()
                        .map(|f| (Bytes::from(f.name), Bytes::from(f.value)))
                        .collect();

                    let has_grpc_status = pairs.iter().any(|(k, _)| k.as_ref() == b"grpc-status");

                    if !got_response_headers && !has_grpc_status {
                        initial_metadata_out = pairs
                            .into_iter()
                            .filter(|(k, _)| !k.starts_with(b":"))
                            .collect();
                        got_response_headers = true;
                    } else {
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
                if let Frame::Data {
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
/// `execute_grpc_unary_call`（同期フォールバック）専用。
#[cfg(test)]
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
    /// panic せず Err を返す。
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

    /// F-134 フォローアップ: 不正なサーバー名（TLS SNI 用）は panic せず Err になること。
    #[test]
    fn test_wrap_tls_invalid_server_name() {
        let _ = rustls::crypto::CryptoProvider::install_default(
            crate::tls_provider::provider::default_provider(),
        );

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

    /// `GrpcUnaryResult`（同期フォールバックの戻り値型）のフィールドを構築・参照できること。
    /// `execute_grpc_unary_call` の他のテストは全て接続失敗（Err）経路のみを検証しており、
    /// Ok 側のフィールドを一切読まないと dead_code 警告になるため、ここで直接検証する。
    #[test]
    fn test_grpc_unary_result_fields() {
        let result = GrpcUnaryResult {
            status_code: 0,
            status_message: String::new(),
            initial_metadata: vec![(
                Bytes::from_static(b"content-type"),
                Bytes::from_static(b"application/grpc"),
            )],
            message: Bytes::from_static(b"payload"),
            trailing_metadata: vec![(Bytes::from_static(b"grpc-status"), Bytes::from_static(b"0"))],
        };
        assert_eq!(result.status_code, 0);
        assert!(result.status_message.is_empty());
        assert_eq!(result.initial_metadata.len(), 1);
        assert_eq!(result.message.as_ref(), b"payload");
        assert_eq!(result.trailing_metadata.len(), 1);
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

        assert!(!cancel_global_pending_grpc_call(
            "test_module_grpc_exec",
            9999
        ));
        assert!(cancel_global_pending_grpc_call("test_module_grpc_exec", 42));

        let taken = take_global_pending_grpc_calls();
        assert!(!taken
            .iter()
            .any(|c| c.module_name == "test_module_grpc_exec" && c.call_id == 42));
    }

    /// F-139: `proxy_grpc_send` 相当の逐次送出登録が、half-close を待たずに
    /// キューへ積まれること（受け入れ条件）。
    #[test]
    fn test_pending_grpc_send_registered_immediately() {
        let send = PendingGrpcSend {
            module_name: "test_module_grpc_send".to_string(),
            call_id: 7,
            upstream: "backend".to_string(),
            path: "/test.Service/Stream".to_string(),
            initial_metadata: GrpcMetadataBlob::empty(),
            timeout_ms: 1000,
            message: Bytes::from_static(b"chunk-1"),
            end_of_stream: false,
        };
        register_global_pending_grpc_send(send);

        let taken = take_global_pending_grpc_sends();
        assert!(taken
            .iter()
            .any(|s| s.module_name == "test_module_grpc_send" && s.call_id == 7));
        // half_close を明示していないので end_of_stream=false のまま。
        assert!(!taken[0].end_of_stream);

        // 取り出し後はキューが空になる。
        assert!(take_global_pending_grpc_sends().is_empty());
    }

    /// F-139: `GrpcRunner` は 1 呼び出し（存在しない upstream）が解決に失敗しても
    /// 他の呼び出しの取り込みには影響しない（状態機械が他の呼び出しをブロックしないこと）。
    #[test]
    fn test_grpc_runner_ingest_unknown_upstream_does_not_block_others() {
        let mut runner = GrpcRunner::new();
        let upstream_groups: HashMap<String, Arc<UpstreamGroup>> = HashMap::new();

        let calls = vec![
            PendingGrpcUnaryCall {
                module_name: "m1".to_string(),
                call_id: 1,
                upstream: "does-not-exist".to_string(),
                path: "/a/b".to_string(),
                initial_metadata: GrpcMetadataBlob::empty(),
                messages: vec![Bytes::from_static(b"x")],
                timeout_ms: 100,
            },
            PendingGrpcUnaryCall {
                module_name: "m2".to_string(),
                call_id: 2,
                upstream: "also-does-not-exist".to_string(),
                path: "/a/b".to_string(),
                initial_metadata: GrpcMetadataBlob::empty(),
                messages: vec![Bytes::from_static(b"y")],
                timeout_ms: 100,
            },
        ];

        let events = runner.ingest(calls, Vec::new(), Vec::new(), &upstream_groups);
        // 両方とも即座に UNIMPLEMENTED で Close される（他方をブロックしない）。
        assert_eq!(events.len(), 2);
        for ev in events {
            match ev {
                GrpcEvent::Close { status_code, .. } => {
                    assert_eq!(status_code, grpc_status::UNIMPLEMENTED);
                }
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert!(!runner.has_active_calls());
    }

    /// F-139: デッドライン超過で `Close(DEADLINE_EXCEEDED)` になること。
    #[test]
    fn test_active_call_deadline_exceeded() {
        // 理由付き allow: 単体テスト（コールドパス）専用の同期 TCP。
        #[allow(clippy::disallowed_methods)]
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        #[allow(clippy::disallowed_methods)]
        let stream = TcpStream::connect(addr).expect("connect should succeed");
        stream.set_nonblocking(true).expect("set_nonblocking");

        let conn = PooledConn::new(ClientStream::Plain(stream), 4096);
        let mut call = ActiveCall {
            module_name: "m".to_string(),
            call_id: 1,
            key: ("h".to_string(), 1, false),
            conn,
            stream_id: 1,
            phase: CallPhase::Streaming,
            outbox: VecDeque::new(),
            end_of_stream_requested: true,
            half_closed_sent: true,
            grpc_dec: GrpcFrameDecoder::new(),
            got_initial_metadata: false,
            stream_send_window: 65535,
            // 既に過去のデッドライン
            deadline: Instant::now() - Duration::from_millis(1),
        };
        let encoder = FrameEncoder::new(16384);
        let decoder = FrameDecoder::new(16384);
        let events = call.poll_once(&encoder, &decoder);
        assert_eq!(events.len(), 1);
        match &events[0] {
            GrpcEvent::Close { status_code, .. } => {
                assert_eq!(*status_code, grpc_status::DEADLINE_EXCEEDED);
            }
            other => panic!("unexpected event: {other:?}"),
        }
        assert_eq!(call.phase, CallPhase::Done);
    }

    // ========================================================================
    // F-139 効果測定: 接続プーリング（`GrpcRunner`）vs 1 呼び出し 1 接続
    // （`execute_grpc_unary_call`、同期フォールバック）
    // ========================================================================
    //
    // `tools/perf` の負荷ハーネスが使う WASM フィルタ（`docker/assets/wasm/`
    // 配下）はヘッダ操作のみで gRPC 呼び出しを一切発行しないため、F-139 が
    // 導入した接続プーリングの効果はどの外形計測にも現れない。ここでは
    // クレート内部の `#[cfg(test)]` からのみ到達できる `execute_grpc_unary_call`
    // （旧方式）と `GrpcRunner`（新方式）を、同一のテスト用 gRPC-over-h2c
    // モックサーバに対して実際に動かし、経過時間と（本質的な効果である）
    // 接続確立回数の両方を測る。

    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// テスト用ミニマル gRPC-over-h2c サーバ。
    ///
    /// プリフェース + SETTINGS を受け付け、以後は同一コネクション上で何度でも
    /// 「ストリームが END_STREAM を受信したらユーナリー応答を返す」を繰り返す
    /// （`GrpcConnPool` による接続再利用＝同一コネクション上の複数呼び出しに
    /// 対応するため）。受理した TCP 接続数を `accepted_conns` に記録する。
    fn spawn_mock_grpc_server(
        accepted_conns: Arc<AtomicUsize>,
    ) -> (u16, std::thread::JoinHandle<()>) {
        // 理由付き allow: 単体テスト（コールドパス）専用の同期 TCP。
        #[allow(clippy::disallowed_methods)]
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind should succeed");
        let port = listener
            .local_addr()
            .expect("local_addr should succeed")
            .port();

        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                accepted_conns.fetch_add(1, Ordering::SeqCst);
                std::thread::spawn(move || {
                    let _ = serve_one_mock_connection(stream);
                });
            }
        });

        (port, handle)
    }

    /// 1 コネクション分の処理。ストリームが END_STREAM を受信するたびに
    /// ユーナリー応答一式（HEADERS → DATA(gRPC framing) → トレーラー HEADERS）を返す。
    fn serve_one_mock_connection(mut stream: TcpStream) -> std::io::Result<()> {
        stream.set_nodelay(true).ok();

        let mut preface_buf = [0u8; 24];
        stream.read_exact(&mut preface_buf)?;
        if preface_buf != *CONNECTION_PREFACE {
            return Ok(());
        }

        let frame_decoder = FrameDecoder::new(16384);
        let frame_encoder = FrameEncoder::new(16384);
        let mut hpack_dec = crate::http2::hpack::HpackDecoder::new(4096);
        let mut hpack_enc = crate::http2::hpack::HpackEncoder::new(4096);

        stream.write_all(&frame_encoder.encode_settings(&[], false))?;

        let mut read_buf = Vec::with_capacity(4096);
        let mut tmp = [0u8; 4096];

        loop {
            while read_buf.len() < FrameHeader::SIZE {
                let n = stream.read(&mut tmp)?;
                if n == 0 {
                    return Ok(());
                }
                read_buf.extend_from_slice(&tmp[..n]);
            }
            let header = match frame_decoder.decode_header(&read_buf) {
                Ok(h) => h,
                Err(_) => return Ok(()),
            };
            let total_len = FrameHeader::SIZE + header.length as usize;
            while read_buf.len() < total_len {
                let n = stream.read(&mut tmp)?;
                if n == 0 {
                    return Ok(());
                }
                read_buf.extend_from_slice(&tmp[..n]);
            }
            let payload = read_buf[FrameHeader::SIZE..total_len].to_vec();
            let end_stream_flag = header.is_end_stream();
            let stream_id = header.stream_id;
            let frame_type = header.get_frame_type();
            read_buf.drain(..total_len);

            match frame_type {
                Some(FrameType::Settings) => {
                    if !header.is_ack() {
                        stream.write_all(&frame_encoder.encode_settings_ack())?;
                    }
                }
                Some(FrameType::Ping) => {
                    if !header.is_ack() && payload.len() == 8 {
                        let mut data = [0u8; 8];
                        data.copy_from_slice(&payload);
                        stream.write_all(&frame_encoder.encode_ping(&data, true))?;
                    }
                }
                Some(FrameType::Headers) | Some(FrameType::Data) => {
                    if frame_type == Some(FrameType::Headers) {
                        let _ = hpack_dec.decode(&payload);
                    }
                    if end_stream_flag {
                        send_mock_unary_response(
                            &mut stream,
                            &frame_encoder,
                            &mut hpack_enc,
                            stream_id,
                        )?;
                    }
                }
                _ => {}
            }
        }
    }

    /// ユーナリー応答一式（HEADERS → DATA(gRPC framing) → トレーラー HEADERS）を送出する。
    fn send_mock_unary_response(
        stream: &mut TcpStream,
        frame_encoder: &FrameEncoder,
        hpack_enc: &mut crate::http2::hpack::HpackEncoder,
        stream_id: u32,
    ) -> std::io::Result<()> {
        let resp_headers: Vec<(&[u8], &[u8], bool)> = vec![
            (b":status", b"200", false),
            (b"content-type", b"application/grpc", false),
        ];
        let header_block = hpack_enc.encode(&resp_headers).expect("hpack encode");
        stream.write_all(&frame_encoder.encode_headers(
            stream_id,
            &header_block,
            false,
            true,
            None,
        ))?;

        // gRPC 5 バイトフレーミング（圧縮なし・長さ 0）+ 空メッセージ。
        let grpc_frame = [0u8, 0, 0, 0, 0];
        stream.write_all(&frame_encoder.encode_data(stream_id, &grpc_frame, false))?;

        let trailer_headers: Vec<(&[u8], &[u8], bool)> = vec![(b"grpc-status", b"0", false)];
        let trailer_block = hpack_enc.encode(&trailer_headers).expect("hpack encode");
        stream.write_all(&frame_encoder.encode_headers(
            stream_id,
            &trailer_block,
            true,
            true,
            None,
        ))?;
        Ok(())
    }

    /// F-139 効果測定本体。旧方式（`execute_grpc_unary_call`: 1 呼び出し 1 接続）と
    /// 新方式（`GrpcRunner`: 接続プール、同一上流への逐次呼び出しは 1 コネクションを
    /// 使い回す）を、同一のモックサーバに対して N = 100 回のユーナリー呼び出しで
    /// 比較する。
    ///
    /// 時間は環境（CI・co-tenant 負荷）でばらつくため `println!` で報告するのみで
    /// assert しない。F-139 の本質である「接続確立回数」（旧 = N 回、新 = 1 回）を
    /// assert する。
    #[test]
    fn measure_pooled_vs_fresh_connection() {
        const N: usize = 100;

        // --- 旧方式: execute_grpc_unary_call（1 呼び出し 1 接続） ---
        let accepted_fresh = Arc::new(AtomicUsize::new(0));
        let (port_fresh, _server_fresh) = spawn_mock_grpc_server(accepted_fresh.clone());
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(Duration::from_millis(50));

        let start_fresh = Instant::now();
        for _ in 0..N {
            let result = execute_grpc_unary_call(
                "127.0.0.1",
                port_fresh,
                false,
                "/bench.Service/Call",
                &GrpcMetadataBlob::empty(),
                &[Bytes::from_static(b"")],
                5000,
            );
            assert!(result.is_ok(), "unary call should succeed: {result:?}");
        }
        let elapsed_fresh = start_fresh.elapsed();
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(Duration::from_millis(100));
        let conns_fresh = accepted_fresh.load(Ordering::SeqCst);

        // --- 新方式: GrpcRunner（接続プール） ---
        let accepted_pooled = Arc::new(AtomicUsize::new(0));
        let (port_pooled, _server_pooled) = spawn_mock_grpc_server(accepted_pooled.clone());
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(Duration::from_millis(50));

        let entry = crate::config::UpstreamServerEntry {
            url: format!("http://127.0.0.1:{port_pooled}"),
            sni_name: None,
            use_h2c: true,
            weight: 1,
        };
        let group = UpstreamGroup::new(
            "bench".to_string(),
            vec![entry],
            crate::config::LoadBalanceAlgorithm::RoundRobin,
            None,
            false,
        )
        .expect("upstream group should build");
        let mut upstream_groups: HashMap<String, Arc<UpstreamGroup>> = HashMap::new();
        upstream_groups.insert("bench".to_string(), Arc::new(group));

        let mut runner = GrpcRunner::new();
        let start_pooled = Instant::now();
        for call_id in 0..N as u32 {
            let call = PendingGrpcUnaryCall {
                module_name: "bench-module".to_string(),
                call_id,
                upstream: "bench".to_string(),
                path: "/bench.Service/Call".to_string(),
                initial_metadata: GrpcMetadataBlob::empty(),
                messages: vec![Bytes::from_static(b"")],
                timeout_ms: 5000,
            };
            let mut events = runner.ingest(vec![call], Vec::new(), Vec::new(), &upstream_groups);
            let mut closed = events
                .iter()
                .any(|e| matches!(e, GrpcEvent::Close { call_id: id, .. } if *id == call_id));
            while !closed {
                events = runner.poll_all();
                closed |= events
                    .iter()
                    .any(|e| matches!(e, GrpcEvent::Close { call_id: id, .. } if *id == call_id));
            }
            // Close 済みなら status_code == OK のはず。
            for ev in &events {
                if let GrpcEvent::Close {
                    status_code,
                    call_id: id,
                    ..
                } = ev
                {
                    if *id == call_id {
                        assert_eq!(
                            *status_code,
                            grpc_status::OK,
                            "call {call_id} should succeed"
                        );
                    }
                }
            }
        }
        let elapsed_pooled = start_pooled.elapsed();
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(Duration::from_millis(100));
        let conns_pooled = accepted_pooled.load(Ordering::SeqCst);

        println!("=== F-139 WASM gRPC 接続プーリング 効果測定（N={N}） ===");
        println!(
            "旧(execute_grpc_unary_call): {:.2} ms（接続確立 {} 回）",
            elapsed_fresh.as_secs_f64() * 1000.0,
            conns_fresh
        );
        println!(
            "新(GrpcRunner):              {:.2} ms（接続確立 {} 回）",
            elapsed_pooled.as_secs_f64() * 1000.0,
            conns_pooled
        );

        // F-139 の本質: 逐次呼び出しなら旧方式は毎回新規接続、新方式は 1 本を使い回す。
        assert_eq!(conns_fresh, N, "旧方式は呼び出し回数だけ接続確立するはず");
        assert_eq!(
            conns_pooled, 1,
            "新方式は接続を再利用し 1 回だけ確立するはず"
        );
    }
}
